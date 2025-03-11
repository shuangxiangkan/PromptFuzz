use std::{process::Child, time::Duration};

use crate::{
    config::{self, get_openai_proxy},
    is_critical_err,
    program::Program,
    FuzzerError,
};
use async_openai::{
    types::{
        ChatCompletionRequestMessage, CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
        CreateChatCompletionResponse, CreateCompletionRequestArgs,
    },
    types::{CreateCompletionRequest, CreateCompletionResponse},
    Client,
};
use eyre::Result;
use once_cell::sync::OnceCell;

use self::openai_billing::{load_openai_usage, log_openai_usage};

use super::{prompt::PromptKind, Handler};

pub struct OpenAIHanler {
    _child: Option<Child>,
    rt: tokio::runtime::Runtime,
}

impl Default for OpenAIHanler {
    fn default() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|_| panic!("Unable to build the openai runtime."));
        //log_account_balance(&rt).unwrap();
        load_openai_usage().unwrap();
        Self { _child: None, rt }
    }
}

impl Handler for OpenAIHanler {
    fn generate_by_str(&self, prefix: &str) -> eyre::Result<Vec<Program>> {
        let _ = prefix;
        unimplemented!("deprecated since no usage")
    }

    fn generate(&self, prompt: &super::prompt::Prompt) -> eyre::Result<Vec<Program>> {
        let start = std::time::Instant::now();
        let chat_msgs = prompt.to_chatgpt_message();
        let mut programs = self.rt.block_on(generate_programs_by_chat(chat_msgs))?;
        for program in programs.iter_mut() {
            program.combination = prompt.get_combination()?;
        }
        log::debug!("LLM Generate time: {}s", start.elapsed().as_secs());
        Ok(programs)
   
    }

    fn infill_by_str(&self, prefix: &str, suffix: &str) -> eyre::Result<Vec<String>> {
        self.rt
            .block_on(generate_infills_by_request(prefix, suffix))
    }

    fn infill(&self, prompt: &super::prompt::Prompt) -> eyre::Result<Vec<String>> {
        let chat_msgs = prompt.to_chatgpt_message();
        if let PromptKind::Infill(_, suffix) = &prompt.kind {
            let stop = suffix[..10].to_string();
            self.rt
                .block_on(generate_infills_by_chat(chat_msgs, Some(stop)))
        } else {
            self.rt.block_on(generate_infills_by_chat(chat_msgs, None))
        }
    }
}

/// Get the OpenAI interface client.
fn get_client() -> Result<&'static Client> {
    // read OpenAI API key form the env var (OPENAI_API_KEY).
    pub static CLIENT: OnceCell<Client> = OnceCell::new();
    let client = CLIENT.get_or_init(|| {
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        let client = if let Some(proxy) = get_openai_proxy() {
            Client::new().with_api_base(proxy)
        } else {
            Client::new()
        };
        client.with_http_client(http_client)
    });
    Ok(client)
}

pub mod openai_billing {
    use std::path::PathBuf;

    use crate::{config::*, deopt::Deopt};
    use super::*;

    fn _get_openai_base() -> &'static str {
        "https://api.openai.com/v1"
    }


    static mut PROMPT_USAGE: u32 = 0;
    static mut COMPLETION_USAGE: u32 = 0;
    pub static mut QUOTA_COST: f32 = 0.0;

    fn get_prompt_token_usage() -> u32 {
        unsafe { PROMPT_USAGE }
    }

    fn get_completion_token_usage() -> u32 {
        unsafe { COMPLETION_USAGE }
    }

    pub fn get_quota_cost() -> f32 {
        unsafe { QUOTA_COST }
    }

    fn get_usage_log_path() -> Result<PathBuf> {
        let deopt = Deopt::new(get_library_name())?;
        let misc_path = deopt.get_library_misc_dir()?;
        let prompt_usage_path: PathBuf = [misc_path, "openai_usage".into()].iter().collect();
        Ok(prompt_usage_path)
    }

    pub fn load_openai_usage() -> Result<()> {
        let log_path = get_usage_log_path()?;
        if log_path.exists() {
            let content = std::fs::read_to_string(log_path)?;
            let parts: Vec<&str> = content.split(' ').collect();
            assert_eq!(parts.len(), 3);
            let prompt_usage: u32 = parts[0].parse()?;
            let completion_usage: u32 = parts[1].parse()?;
            let quota_cost: f32 = parts[2].parse()?;
            unsafe { PROMPT_USAGE = prompt_usage };
            unsafe { COMPLETION_USAGE = completion_usage };
            unsafe { QUOTA_COST = quota_cost };
        }
        Ok(())
    }

    pub fn log_openai_usage(response: &CreateChatCompletionResponse) -> Result<()> {
        if let Some(usage) = &response.usage {
            let prompt_token = usage.prompt_tokens;
            let complete_token = usage.completion_tokens;
            let log_path = get_usage_log_path()?;

            unsafe {
                PROMPT_USAGE += prompt_token;
            }
            unsafe {
                COMPLETION_USAGE += complete_token;
            }
            count_billing(prompt_token, complete_token)?;
            let content: String = [
                get_prompt_token_usage().to_string(),
                " ".into(),
                get_completion_token_usage().to_string(),
                " ".into(),
                get_quota_cost().to_string(),
            ]
            .concat();
            std::fs::write(log_path, content)?;
        }
        Ok(())
    }

    fn count_billing(prompt_usage: u32, completion_usage: u32) -> Result<()> {
        let input_price = get_openai_input_price();
        let output_price = get_openai_output_price();
        if input_price.is_none() || output_price.is_none() {
            return Ok(());
        }

        let input_price = input_price.unwrap();
        let output_price = output_price.unwrap();

        let curr_fee =
            (input_price * prompt_usage as f32) + (output_price * completion_usage as f32);
        let curr_fee = curr_fee / 1000_f32;
        unsafe {
            QUOTA_COST += curr_fee;
        }
        let current_cost = get_quota_cost();
        log::info!("Total OPENAI corpora cost: ${current_cost}");
        Ok(())
    }
}

async fn get_complete_response(
    request: CreateCompletionRequest,
) -> Result<CreateCompletionResponse> {
    let client = get_client().unwrap();
    for _retry in 0..config::RETRY_N {
        let response = client
            .completions()
            .create(request.clone())
            .await
            .map_err(eyre::Report::new);
        match is_critical_err(&response) {
            crate::Critical::Normal => return response,
            crate::Critical::NonCritical => continue,
            crate::Critical::Critical => return Err(response.err().unwrap()),
        }
    }
    Err(FuzzerError::RetryError(format!("{request:?}"), config::RETRY_N).into())
}

/// Create a request for a chat prompt
fn create_chat_request(
    msgs: Vec<ChatCompletionRequestMessage>,
    stop: Option<String>,
) -> Result<CreateChatCompletionRequest> {
    let mut binding = CreateChatCompletionRequestArgs::default();
    let binding = binding.model(config::get_openai_model_name());

    let mut request = binding
        .messages(msgs)
        .n(config::get_sample_num())
        .temperature(config::get_config().temperature)
        .stream(false);
    if let Some(stop) = stop {
        request = request.stop(stop);
    }
    let request = request.build()?;
    Ok(request)
}

/// Get a response for a chat request
async fn get_chat_response(
    request: CreateChatCompletionRequest,
) -> Result<CreateChatCompletionResponse> {
    let client = get_client().unwrap();
    for _retry in 0..config::RETRY_N {
        let response = client
            .chat()
            .create(request.clone())
            .await
            .map_err(eyre::Report::new);
        match is_critical_err(&response) {
            crate::Critical::Normal => {
                let response = response?;
                log_openai_usage(&response)?;
                return Ok(response);
            }
            crate::Critical::NonCritical => {
                continue;
            }
            crate::Critical::Critical => return Err(response.err().unwrap()),
        }
    }
    Err(FuzzerError::RetryError(format!("{request:?}"), config::RETRY_N).into())
}

fn create_infill_request(prefix: &str, suffix: &str) -> Result<CreateCompletionRequest> {
    let request = CreateCompletionRequestArgs::default()
        .model(config::get_openai_model_name())
        .prompt(prefix)
        .suffix(suffix)
        .max_tokens(config::MAX_TOKENS)
        .n(config::get_sample_num())
        .temperature(config::get_config().temperature)
        .stream(false)
        .build()?;
    Ok(request)
}

/// Generate `SAMPLE_N` programs by chatting with instructions.
pub async fn generate_programs_by_chat(
    chat_msgs: Vec<ChatCompletionRequestMessage>,
) -> Result<Vec<Program>> {
    let request = create_chat_request(chat_msgs, None)?;
    let respond = get_chat_response(request).await?;
    if let Some(usage) = &respond.usage {
        log::trace!("Corpora usage: {usage:?}");
    }
    let mut programs: Vec<Program> = Vec::new();
    for choice in respond.choices {
        let content = choice.message.content;
        let content = strip_code_wrapper(&content);
        let program = Program::new(&content);
        programs.push(program);
    }
    Ok(programs)
}

/// Generate `INFILL_N` infills for the given prefix and suffix.
async fn generate_infills_by_request(prefix: &str, suffix: &str) -> Result<Vec<String>> {
    let request = create_infill_request(prefix, suffix)?;
    let respond = get_complete_response(request).await?;
    if let Some(usage) = &respond.usage {
        log::trace!("Corpora usage: {usage:?}");
    }
    let mut infills: Vec<String> = Vec::new();
    for choice in respond.choices {
        infills.push(choice.text.clone())
    }
    Ok(infills)
}

/// Generate `INFILL_N` infills by chatting with instructions.
async fn generate_infills_by_chat(
    chat_msgs: Vec<ChatCompletionRequestMessage>,
    stop: Option<String>,
) -> Result<Vec<String>> {
    let request = create_chat_request(chat_msgs, stop)?;
    let respond = get_chat_response(request).await?;
    if let Some(usage) = &respond.usage {
        log::trace!("Corpora usage: {usage:?}");
    }
    let mut infills = vec![];
    for choice in respond.choices {
        let content = choice.message.content;
        let content = strip_code_wrapper(&content);
        infills.push(content);
    }
    Ok(infills)
}

fn strip_code_prefix<'a>(input: &'a str, pat: &str) -> &'a str {
    let pat = String::from_iter(["```", pat]);
    if input.starts_with(&pat) {
        if let Some(p) = input.strip_prefix(&pat) {
            return p;
        }
    }
    input
}

/// strip the code wrapper that ChatGPT generated with code.
fn strip_code_wrapper(input: &str) -> String {
    let mut input = input.trim();
    let mut event = "";
    if let Some(idx) = input.find("```") {
        event = &input[..idx];
        input = &input[idx..];
    }
    let input = strip_code_prefix(input, "cpp");
    let input = strip_code_prefix(input, "CPP");
    let input = strip_code_prefix(input, "C++");
    let input = strip_code_prefix(input, "c++");
    let input = strip_code_prefix(input, "c");
    let input = strip_code_prefix(input, "C");
    let input = strip_code_prefix(input, "\n");
    if let Some(idx) = input.rfind("```") {
        let input = &input[..idx];
        let input = ["/*", event, "*/\n", input].concat();
        return input;
    }
    ["/*", event, "*/\n", input].concat()
}

