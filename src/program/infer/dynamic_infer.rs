/// Dynamically infer the constraints of AllocSize, LoopCount and so on influence the performance.
use std::{
    ffi::{OsStr, OsString},
    sync::RwLock,
};

use eyre::Context;
use once_cell::sync::OnceCell;

use crate::{
    deopt::utils::get_file_dirname,
    execution::{logger::ProgramError, Executor},
    feedback::{
        clang_coverage::{utils::sanitize_by_fuzzer_coverage, CodeCoverage},
        observer::Observer,
    },
    program::{
        gadget::{
            ctype::{get_integer_ty_max, get_integer_ty_min},
            get_func_gadget, get_func_gadgets,
        },
        transform::Transformer,
        Program,
    },
};

use super::*;

impl<'a> Transformer<'a> {
    fn new_infer(program_id: usize, deopt: &'a Deopt) -> Result<Self> {
        let work_seed = deopt.get_work_seed_by_id(program_id)?;
        let src_path: PathBuf = work_seed.with_extension("infer.cc");
        std::fs::copy(work_seed, &src_path)?;
        let transformer = Self::new(&src_path, deopt)?;
        Ok(transformer)
    }

    fn change_call_arg_to_value(
        &mut self,
        func: &str,
        arg_pos: usize,
        new_value: &str,
    ) -> Result<()> {
        let visitor = self.get_new_visitor()?;
        if let Some(call) = visitor.find_callexpr(func, 0) {
            self.change_call_arg(call, arg_pos, new_value)?;
        } else {
            eyre::bail!("no call: {func}")
        }
        Ok(())
    }

    fn get_infer_program(&self) -> (PathBuf, PathBuf) {
        let program = self.src_file.clone();
        let dir = get_file_dirname(&program);
        let binary: PathBuf = [dir, "fuzz_infer.out".into()].iter().collect();
        (program, binary)
    }
}

impl Executor {
    pub fn transform_check(
        &mut self,
        driver: &Path,
        corpora: &Path,
    ) -> Result<Option<ProgramError>> {
        self.deopt
            .prepare_transform_path_for_driver(driver, corpora)?;

        let check_dir = get_file_dirname(driver);
        let fuzz_binary: PathBuf = [check_dir.clone(), "fuzzer".into()].iter().collect();
        self.compile(
            vec![driver],
            &fuzz_binary,
            crate::execution::Compile::FUZZER,
        )?;

        let corpus: PathBuf = [check_dir, "corpus".into()].iter().collect();
        let max_total_time = "-max_total_time=60".to_string();
        let mut extra_args = vec![
            corpus.as_os_str().to_os_string(),
            OsString::from(max_total_time),
        ];
        let dict = self.deopt.get_library_build_dict_path().unwrap();
        if dict.exists() {
            let dict_arg = format!("-dict={}", dict.to_string_lossy());
            extra_args.push(OsString::from(dict_arg));
        }
        let has_err = self.execute(&fuzz_binary, extra_args, vec![], None, None, false)?;
        if let Some(ProgramError::Hang(err_msg)) = &has_err {
            if err_msg.starts_with("Execute hang!") {
                return Ok(None);
            }
        }
        Ok(has_err)
    }
}

pub fn infer(static_constraint: &mut APIConstraints, deopt: &Deopt) -> Result<()> {
    log::info!("Dynamically infer the constraints imposed on integral.");

    let mut dyn_constraints: APIConstraints = APIConstraints::new();
    for (func, args) in find_exploitable_func(static_constraint)? {
        log::info!("infer constraint for: {func} and args: {args:?}");
        let mut func_constraints = Vec::new();
        let test_programs = find_testbed_program(&func, deopt);
        for program in test_programs {
            log::info!("infer constraint on program: {program:?}");
            for arg in &args {
                let has_constraint = infer_constraint_for_func(&func, *arg, program, deopt)?;
                if let Some(constraint) = has_constraint {
                    log::debug!("Infered! {func}, {constraint:?}");
                    func_constraints.push(constraint);
                }
            }
        }
        func_constraints = dedup_constraint(func_constraints);
        dyn_constraints.insert(func, func_constraints);
    }
    for (func, cs) in dyn_constraints {
        if cs.is_empty() {
            continue;
        }
        if let Some(static_cs) = static_constraint.get_mut(&func) {
            static_cs.extend(cs);
        } else {
            static_constraint.insert(func, cs);
        }
    }
    Ok(())
}

pub fn infer_constraint_for_func(
    func: &str,
    arg_pos: usize,
    program_id: usize,
    deopt: &Deopt,
) -> Result<Option<Constraint>> {
    let gadget = get_func_gadget(func).expect("expect a gadget");
    if let Some(arg_ty) = gadget.get_canonical_arg_type(arg_pos) {
        let max_value = get_integer_ty_max(arg_ty);
        let min_value = get_integer_ty_min(arg_ty);
        let magic_value = format!("({max_value} + {min_value}) / 0x2000");
        let has_err =
            infer_constraint_for_func_by_value(func, arg_pos, program_id, deopt, &max_value)?;
        if has_err.is_some() {
            return Ok(has_err);
        }
        let has_err =
            infer_constraint_for_func_by_value(func, arg_pos, program_id, deopt, &min_value)?;
        if has_err.is_some() {
            return Ok(has_err);
        }
        let has_err =
            infer_constraint_for_func_by_value(func, arg_pos, program_id, deopt, &magic_value)?;
        if has_err.is_some() {
            return Ok(has_err);
        }
    }
    Ok(None)
}

fn infer_constraint_for_func_by_value(
    func: &str,
    arg_pos: usize,
    program_id: usize,
    deopt: &Deopt,
    new_value: &str,
) -> Result<Option<Constraint>> {
    log::trace!("infer constraint for arg_pos {arg_pos} of {func}. value: {new_value}");
    let mut transformer = Transformer::new_infer(program_id, deopt)?;
    transformer.change_call_arg_to_value(func, arg_pos, new_value)?;
    let (program, binary) = transformer.get_infer_program();

    let executor = Executor::new(deopt)?;
    let corpus = deopt.get_library_shared_corpus_dir()?;
    executor.compile(vec![&program], &binary, crate::execution::Compile::FUZZER)?;

    let has_err = executor.execute_pool(&binary, &corpus);
    if let Some(err) = has_err {
        if matches!(err, ProgramError::Hang(_)) {
            let constraint = Constraint::LoopCount(arg_pos);
            return Ok(Some(constraint));
        }
        if let ProgramError::Execute(err_msg) = err {
            if err_msg.contains("out-of-memory")
                || err_msg.contains("allocator is trying to allocate")
                || err_msg.contains("exceeds maximum supported size of")
                || err_msg.contains("allocation-size-too-big")
            {
                let constraint = Constraint::AllocSize(arg_pos);
                return Ok(Some(constraint));
            }
            if err_msg.contains("SEGV") || err_msg.contains("overflow") {
                let constraint = Constraint::ArrayIndex((usize::MAX, arg_pos));
                return Ok(Some(constraint));
            }
        }
    }
    Ok(None)
}

/// Find the api functions that contains an fuzzable integeral parameter.
fn find_exploitable_func(static_constraint: &APIConstraints) -> Result<Vec<(String, Vec<usize>)>> {
    let mut funcs = Vec::new();
    for gadget in get_func_gadgets() {
        let api_name = gadget.get_func_name();
        let mut int_pos = gadget.get_integer_params_pos();
        // remove the param that constrainted by ArrayLen and ArrayIndex and FileDesc
        if let Some(constraints) = static_constraint.get(api_name) {
            for constraint in constraints {
                if let Some((_, arg_pos)) = constraint.get_arg_tuple() {
                    int_pos.retain(|x| x != arg_pos);
                }
                if let Constraint::FileDesc(arg_pos) = constraint {
                    int_pos.retain(|x| x != arg_pos);
                }
            }
        }
        if int_pos.is_empty() {
            continue;
        }
        funcs.push((api_name.to_string(), int_pos));
    }
    Ok(funcs)
}

/// Ranked API coverage for API in each programs.
type APICovRank = HashMap<String, Vec<(f32, usize)>>;

fn get_api_cov_map(deopt: &Deopt) -> &'static APICovRank {
    static GLOBAL_API_COV: OnceCell<APICovRank> = OnceCell::new();
    GLOBAL_API_COV.get_or_init(|| {
        eval_api_cov_map(deopt)
            .context("cannot rank api coverage in seeds")
            .unwrap()
    })
}

fn eval_api_cov_map(deopt: &Deopt) -> Result<APICovRank> {
    let mut global_rank: APICovRank = APICovRank::new();
    let seed_dir = deopt.get_library_succ_seed_dir()?;

    for seed_path in crate::deopt::utils::read_sort_dir(&seed_dir)? {
        let seed_program = Program::load_from_path(&seed_path)?;
        let seed_id = seed_program.id;
        let coverage_file = deopt.get_seed_coverage_file(seed_id)?;
        if !coverage_file.exists() {
            let executor = Executor::new(deopt)?;
            executor.eval_seed_coverage(seed_id)?;
        }
        let coverage = deopt
            .get_seed_coverage(seed_id)
            .context("cannot find coverage file")?;
        let mut observer = Observer::from_coverage(&coverage, deopt);
        let api_coverage = observer.compute_library_api_coverage()?;
        for (api, cov) in api_coverage {
            if !seed_program.get_quality().library_calls.contains(api) {
                continue;
            }
            let entry = global_rank.entry(api.to_string()).or_default();
            entry.push((*cov, seed_id));
        }
    }
    for value in global_rank.values_mut() {
        value.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Less));
        value.reverse();
    }
    Ok(global_rank)
}

/// Find a program that achieved the most coverage of an API.
fn find_testbed_program(func: &str, deopt: &Deopt) -> Vec<usize> {
    let g_cov_map = get_api_cov_map(deopt);
    let limit = 3;
    if let Some(vector) = g_cov_map.get(func) {
        let mut select = vec![];
        for (i, (_, seed_id)) in vector.iter().enumerate() {
            if i >= limit {
                break;
            }
            select.push(*seed_id);
        }
        return select;
    }
    vec![]
}

fn get_corpora_coverage(
    fuzzer_code: &Path,
    fuzzer_cov: &Path,
    corpora: &Path,
    executor: &Executor,
) -> Result<CodeCoverage> {
    let work_dir = get_file_dirname(fuzzer_code);
    let profraw: PathBuf = [work_dir.clone(), "check.profraw".into()].iter().collect();
    let profdata: PathBuf = [work_dir, "check.profdata".into()].iter().collect();

    let extra_args = vec![corpora.as_os_str()];
    let extra_envs = vec![(OsStr::new("LLVM_PROFILE_FILE"), profraw.as_os_str())];

    let has_err = executor.execute(fuzzer_cov, extra_args, extra_envs, None, None, false)?;
    if has_err.is_some() {
        eyre::bail!(
            "Find testbed corpora should not fail! Binary: {fuzzer_cov:?}, File: {corpora:?}, ERR: {has_err:?}"
        )
    }
    executor.convert_profraw_to_profdata(&profraw, &profdata)?;
    let cov = executor.obtain_cov_from_profdata(&profdata)?;
    let lcov = executor.obtain_fuzzer_cov_from_profdata(&profdata, fuzzer_code, fuzzer_cov)?;
    let cov = cov.set_fuzzer_lines(lcov);
    Ok(cov)
}

/// Find a testbed corpora for a program. The testbed corpora should be good enough to cover the program's code.
pub fn find_testbed_corpora(program_path: &Path, deopt: &Deopt) -> Result<PathBuf> {
    log::debug!("Find testbed corpora for program: {:?}", program_path);
    static CACHE: OnceCell<RwLock<Vec<PathBuf>>> = OnceCell::new();
    let cache = CACHE.get_or_init(|| RwLock::new(Vec::new()));

    let executor = Executor::new(deopt)?;
    let seed_id = Program::load_from_path(program_path)?.id;
    executor.compile_seed(seed_id)?;
    let fuzzer_code = deopt.get_work_seed_by_id(seed_id)?;
    let work_dir = get_file_dirname(&fuzzer_code);
    let fuzzer_cov: PathBuf = fuzzer_code.with_extension("cov.out");

    let mut ranked_files = Vec::new();

    for cache_file in cache.read().unwrap().iter() {
        let cov = get_corpora_coverage(&fuzzer_code, &fuzzer_cov, cache_file, &executor);
        if let Err(err) = cov {
            log::error!("{err}");
            break;
        }
        let cov = cov?;
        let cov_score = cov.get_total_summary().count_covered_branches();
        ranked_files.push((cache_file.to_path_buf(), cov_score));
        if !sanitize_by_fuzzer_coverage(&fuzzer_code, deopt, &cov)? {
            return Ok(cache_file.to_path_buf());
        }
    }

    // minimize the shared corpus first to reduce time cost.
    let corpus_dir: PathBuf = [work_dir.clone(), "corpus".into()].iter().collect();
    if corpus_dir.exists() {
        std::fs::remove_dir_all(&corpus_dir)?;
    }
    let fuzzer_bin = fuzzer_code.with_extension("out");
    executor.minimize_corpus(
        &fuzzer_bin,
        &corpus_dir,
        &executor.deopt.get_library_shared_corpus_dir()?,
    )?;

    let corpus_files = crate::deopt::utils::read_all_files_in_dir(&corpus_dir)?;

    let mut max_branch = 0;
    let mut max_corpora = None;
    for corpora in corpus_files {
        let cov = get_corpora_coverage(&fuzzer_code, &fuzzer_cov, &corpora, &executor);
        if let Err(err) = cov {
            log::error!("{err}");
            break;
        }
        let cov = cov?;
        if sanitize_by_fuzzer_coverage(&fuzzer_code, deopt, &cov)? {
            continue;
        }
        let covered_branch = cov.get_total_summary().count_covered_branches();
        if covered_branch > max_branch {
            max_branch = covered_branch;
            max_corpora = Some(corpora.clone());
        }
    }
    if let Some(corpora) = max_corpora {
        cache.write().unwrap().push(corpora.clone());
        return Ok(corpora);
    }
    if !ranked_files.is_empty() {
        ranked_files.sort_by_key(|a| a.1);
        let corpora = ranked_files
            .last()
            .expect("ranked files could not be empty")
            .0
            .clone();
        return Ok(corpora);
    }
    if  !cache.read().unwrap().is_empty()  {
        let choose = cache.read().unwrap().first().unwrap().to_path_buf() ;
        return Ok(choose);
    }
    eyre::bail!("Cannot find the corpora that statisfy a good coverage")
}

fn dedup_constraint(constraints: Vec<Constraint>) -> Vec<Constraint> {
    let mut new = Vec::new();
    let mut set = Vec::new();
    for constraint in constraints {
        let arg = constraint.get_integer_arg();
        if set.contains(&arg) {
            continue;
        }
        new.push(constraint);
        set.push(arg);
    }
    new
}

#[test]
fn test_dynamic_infer() -> Result<()> {
    crate::config::Config::init_test("zlib");
    let deopt = Deopt::new("zlib".to_string())?;

    let id = 1429;
    let func = "gzfread";
    let arg_pos = 1;
    let has_constraint = infer_constraint_for_func(&func, arg_pos, id, &deopt)?;
    println!("{has_constraint:?}");

    Ok(())
}
