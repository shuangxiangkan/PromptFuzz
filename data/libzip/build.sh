#!/bin/bash

source ../common.sh

PROJECT_NAME=libzip
STALIB_NAME=libzip.a
DYNLIB_NAME=libzip.so
DIR=$(pwd)


function download() {
    cd $SRC
    if [ -x "$(command -v coscli)" ]; then
        coscli cp cos://sbd-testing-1251316161/bench_archive/LLM_FUZZ/archives/libzip.tar.gz libzip.tar.gz
        tar -xvf libzip.tar.gz && rm libzip.tar.gz
    else
        git clone --depth 1 https://github.com/nih-at/libzip.git
    fi
    # Don't try to rename if the directory name is already correct
    if [ "$(basename $(pwd))/libzip" != "$(basename $(pwd))/$PROJECT_NAME" ]; then
        mv libzip $PROJECT_NAME
    fi
}

function build_lib() {
    LIB_STORE_DIR=$WORK/build
    rm -rf $LIB_STORE_DIR
    mkdir -p $LIB_STORE_DIR
    cd $LIB_STORE_DIR
    # 添加缺失的依赖库安装
    apt-get install -y libbz2-dev liblzma-dev libzstd-dev
    cmake -DBUILD_SHARED_LIBS=OFF \
          -DENABLE_GNUTLS=OFF \
          -DENABLE_MBEDTLS=OFF \
          -DENABLE_OPENSSL=ON \
          -DBUILD_TOOLS=OFF \
          -DHAVE_CRYPTO=ON \
          $SRC/libzip
    make -j$(nproc)
    
    # 添加：复制库文件到当前目录
    cp lib/libzip.a .
    cp lib/libzip.so . 2>/dev/null || true  # 如果有动态库也复制
}

function build_oss_fuzz() {
    cd $LIB_STORE_DIR
    
    # 排除fuzz_main.c避免main函数冲突
    for fuzzer in $(find $SRC/libzip/ossfuzz -name "*.c" ! -name "fuzz_main.c" -exec basename {} \; | sed 's/\.c$//')
    do
        echo "Building fuzzer: $fuzzer"
        $CC $CFLAGS -I. -I$SRC/libzip/lib \
            $SRC/libzip/ossfuzz/$fuzzer.c \
            -o $OUT/$fuzzer \
            $LIB_FUZZING_ENGINE ${LIB_STORE_DIR}/libzip.a \
            -L/usr/lib/x86_64-linux-gnu \
            -lbz2 -llzma -lz -lzstd -lssl -lcrypto
    done
}

function copy_include() {
    mkdir -p ${LIB_BUILD}/include
    # 复制源码中的zip.h
    cp ${SRC}/${PROJECT_NAME}/lib/zip.h ${LIB_BUILD}/include/
}

function build_corpus() {
    mkdir -p ${LIB_BUILD}/corpus
    
    # Copy the actual corpus files from the regress directory as done in OSS-Fuzz
    find $SRC/libzip/regress -name "*zip" -exec cp {} ${LIB_BUILD}/corpus/ \;
    
    # Also copy the seed corpus for the encrypt fuzzer if it exists
    if [ -f "$SRC/libzip/ossfuzz/zip_write_encrypt_aes256_file_fuzzer_seed_corpus.zip" ]; then
        cp $SRC/libzip/ossfuzz/zip_write_encrypt_aes256_file_fuzzer_seed_corpus.zip ${LIB_BUILD}/corpus/
    fi
}

function build_dict() {
    # Copy the actual dictionary file from OSS-Fuzz
    if [ -f "$SRC/libzip/ossfuzz/zip_read_fuzzer.dict" ]; then
        cp $SRC/libzip/ossfuzz/zip_read_fuzzer.dict ${LIB_BUILD}/fuzzer.dict
    else
        # Create a fallback dictionary if the original is not available
        echo "# libzip dictionary" > ${LIB_BUILD}/fuzzer.dict
        echo "\"PK\"" >> ${LIB_BUILD}/fuzzer.dict
        echo "\"zip\"" >> ${LIB_BUILD}/fuzzer.dict
        echo "\"unzip\"" >> ${LIB_BUILD}/fuzzer.dict
        echo "\"archive\"" >> ${LIB_BUILD}/fuzzer.dict
        echo "\"central directory\"" >> ${LIB_BUILD}/fuzzer.dict
        echo "\"local file header\"" >> ${LIB_BUILD}/fuzzer.dict
    fi
}

build_all 