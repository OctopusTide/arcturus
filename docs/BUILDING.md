# Windows 构建

需要支持 edition 2024 的 Rust 工具链；`Cargo.lock` 固定依赖版本。先配置构建工具，再运行 Cargo。构建不会安装服务、自启或 hook。

- MSVC：需要 `x86_64-pc-windows-msvc` 工具链、Visual Studio C++ Build Tools 和 Windows SDK，在开发者终端构建。
- GNU：需要 `x86_64-pc-windows-gnu` 工具链及匹配架构的 MinGW-w64。其 `bin` 目录必须包含 `gcc.exe`、`ar.exe`、`dlltool.exe` 并加入本次终端 PATH；仅安装 Rust 不够。

`windows-sys 0.61` 通过 `windows-link` 使用 GNU 的 `dlltool`。其他依赖也引用此版本，单独降级一个包不能解决缺少工具的问题。用 `Get-Command rustc,cargo,dlltool,gcc` 和 `rustc -vV` 检查实际工具链。

## 编译和测试

```powershell
cargo build --workspace --locked
cargo test --workspace --locked
```

离线构建另加 `--offline`，前提是依赖已缓存。想把缓存和临时文件放到别的盘，先设 `CARGO_HOME`、`TEMP`、`TMP`。`target` 留在项目内。切换工具链或工具路径只需修改当前终端 PATH，不需改变机器全局配置。

若 GNU 链接器无法读取非 ASCII 缓存路径里的 `libwindows.0.52.0.a`，将**当前锁定依赖自带的同一份库**复制到项目内只含 ASCII 的库目录，并将该目录加入本次构建的 `LIBRARY_PATH`；不要下载来源不明的导入库。MSVC 不使用该 GNU 导入库。

服务安装另见 [管理员服务](ADMIN_SERVICE.md)。
