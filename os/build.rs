//! Building applications linker

use std::fs::{read_dir, File};
use std::io::{Result, Write};

fn main() {
    println!("cargo:rerun-if-changed=../user/src/");
    println!("cargo:rerun-if-changed={}", TARGET_PATH);
    insert_app_data().unwrap();
}

static TARGET_PATH: &str = "../user/build/bin/";

/// 生成应用程序链接脚本
///
/// 本函数扫描指定目录下的用户程序二进制文件，生成汇编链接脚本 `link_app.S`，
/// 用于在操作系统启动时将用户程序嵌入内核镜像，并建立符号表供内核加载应用。
///
/// # 流程
/// 1. 扫描 `../user/build/bin/` 目录下的所有 `.bin` 文件
/// 2. 提取文件名并排序（确保链接顺序确定）
/// 3. 生成全局符号表：
///    - `_num_app`: 应用总数
///    - `app_{n}_start`/`app_{n}_end`: 每个应用的起始/结束地址
/// 4. 使用 `.incbin` 指令将二进制文件嵌入汇编
///
/// # 返回值
/// 返回 `Result<()>` 表示操作结果，可能包含 IO 错误
///
/// # 注意
/// - 生成的 `link_app.S` 需通过链接脚本引入内核
/// - 要求用户程序编译为 `.bin` 格式并位于指定目录
/// get app data and build linker
fn insert_app_data() -> Result<()> {
    let mut f: File = File::create("src/link_app.S").unwrap();
    let mut apps: Vec<_> = read_dir("../user/build/bin/")
        .unwrap()
        .into_iter()
        .map(|dir_entry| {
            let mut name_with_ext = dir_entry.unwrap().file_name().into_string().unwrap();
            // 去除文件扩展名（如 "hello.bin" -> "hello"）
            name_with_ext.drain(name_with_ext.find('.').unwrap()..name_with_ext.len());
            name_with_ext
        })
        .collect();
    apps.sort();

    writeln!(
        f,
        r#"
    .align 3
    .section .data
    .global _num_app
_num_app:
    .quad {}"#,
        apps.len()
    )?;

    for i in 0..apps.len() {
        writeln!(f, r#"    .quad app_{}_start"#, i)?;
    }
    writeln!(f, r#"    .quad app_{}_end"#, apps.len() - 1)?;

    for (idx, app) in apps.iter().enumerate() {
        println!("app_{}: {}", idx, app);
        writeln!(
            f,
            r#"
    .section .data
    .global app_{0}_start
    .global app_{0}_end
app_{0}_start:
    .incbin "{2}{1}.bin"
app_{0}_end:"#,
            idx, app, TARGET_PATH
        )?;
    }
    Ok(())
}
