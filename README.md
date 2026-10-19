# PostgreSQL 老版本兼容驱动（DBX 插件）

[English](README.en.md) | 中文

一个纯 Rust 实现的 [DBX](https://github.com/t8y2/dbx) 外部驱动插件。它让 DBX 可以正常使用 **PostgreSQL 8.3 ~ 9.0** 等老版本服务器——无需 Java、无需 JDBC 驱动 JAR、无需任何运行时依赖。

## 解决什么问题

DBX 内置的 PostgreSQL 连接在现代服务器上工作良好，但面对 PostgreSQL 8.3 会失败：

| DBX 行为 | 8.3 上的结果 |
| --- | --- |
| 表数据预览生成 `left(col, 8193)` | `left()`/`right()` 是 **9.1** 才引入的，报 `function left(text, integer) does not exist`（SQLSTATE 42883） |
| 元数据查询使用 `format()`（9.1）、`row_to_json`（9.2）、`LATERAL`（9.3）、`unnest WITH ORDINALITY`（9.4）等 | 各类 "does not exist" 错误，库/表/列树无法浏览 |

本插件注册为 DBX 的 **JDBC 外部驱动**，自己实现 PostgreSQL 3.0 线协议（8.3 原生讲的语言），并在两个层面保证兼容：

1. **SQL 改写器**（`src/rewrite83.rs`）：所有发往服务器的 SQL 先经过改写——
   - `left(e, n)`  → `(CASE WHEN (n) < 1 THEN substring(e from 1 for 0) ELSE substring(e from 1 for (n)) END)`
   - `right(e, n)` → 等价的 `substring` CASE 展开
   - 字符串字面量、带引号标识符、注释、`pg_catalog.left(...)` 限定调用、JOIN 关键字、名为 `left` 的列全部原样保留
2. **8.3 时代的元数据 SQL**（`src/meta83.rs`）：每一条查询只用 8.3 及更早就存在的目录表与函数，并对照 psql 8.3 自身的查询方式逐条核验（`oidvectortypes` 签名、`pg_get_constraintdef` 主键、`pg_proc` 重建函数源码）。单测断言不出现任何 8.4+/9.x+ 特性。

## 安装

### 方式一：下载发布包

到 [Releases](../../releases) 下载对应平台的压缩包，解压后把 `dbx-pg83-compat/` 整个目录放到 DBX 插件目录：

| DBX 形态 | 插件目录 |
| --- | --- |
| **Windows 安装版**（exe 旁有 `uninstall.exe`，最常见） | `%APPDATA%\com.dbx.app\plugins\` |
| Windows 便携版（exe 旁有 `portable.dbx`） | `<DBX所在目录>\data\plugins\` |
| 设置里自定义过插件存储目录 | 以设置里的路径为准 |
| CLI / MCP（`dbx` 命令行） | `~/.dbx/plugins/` |

### 方式二：从源码构建

```bash
git clone https://github.com/cyh2002-408/dbx-pg83-compat.git
cd dbx-pg83-compat
cargo build --release
cargo test          # 32 个单元测试
python smoke/smoke.py   # 端到端 smoke（需 Python 3）
```

然后把 `target/release/dbx-pg83-driver`（Windows 为 `.exe`）按下述结构放入插件目录，或直接运行 `install.ps1`（Windows）。

### 目录结构

```
<插件目录>/dbx-pg83-compat/
├── manifest.json
└── bin/
    ├── dbx-pg83-driver.bat      ← Windows 上 DBX 加载这个
    └── dbx-pg83-driver.exe
```

Linux/macOS 布局：`bin/dbx-pg83-driver` 为无扩展名的原生二进制（可执行权限）。

## 使用

1. **完全重启 DBX**（注意托盘——关闭按钮默认是最小化）。
2. 新建连接，类型选 **JDBC**，URL 填：`jdbc:postgresql://主机:5432/数据库名`
3. 用户名 / 密码按服务器实际填写（写在 URL 的 `?user=..&password=..` 里也可以）。

之后左侧库/schema/表浏览、数据预览（大字段自动截断预览）、SQL 执行、手动事务、语句取消、EXPLAIN 都走本插件。

**卸载**：删除插件目录即可，不影响 DBX 其他连接。

## 已实现与已知限制

**DBX 外部驱动协议**（与官方 JDBC 插件同一通道）：
`testConnection` / `connect` / `connectionInfo` / `disconnect` / `executeQuery` / `executeQueryPage` / `fetchQueryPage` / `closeQuerySession` / `beginManualTransaction` / `executeInManualTransaction` / `commitManualTransaction` / `rollbackManualTransaction` / `cancelStatement` / `listDatabases` / `listSchemas` / `listTables` / `listObjects` / `listDataTypes` / `listIndexes` / `getColumns` / `getObjectSource` / `getExplainInfo`

**PostgreSQL 线协议**：protocol 3.0，trust / password / md5 认证，simple query，文本格式传值，UTF-8 客户端编码。

**已知限制**：
- 不支持 SSL（`sslmode=require` 的服务器请先放行非加密连接）。8.3 时代的常见部署是内网明文。
- 不支持 SCRAM（PG 10+ 才有；8.3 只会有 md5/password，正好覆盖）。
- `getObjectSource` 支持 VIEW / FUNCTION / TRIGGER / SEQUENCE；函数源码是从 `pg_proc` 重建的近似定义（8.4 前没有 `pg_get_functiondef`），用于查看而非回放；序列的 `START WITH` 在 8.3 目录中不记录，以注释形式附当前值。
- 每个数据库连接是独立 sidecar 进程（DBX 的 JDBC 池模型如此）。

## 工作原理

```
DBX 主程序
   │  db_type = JDBC → external_driver_pool("jdbc")
   ▼
manifest.json (v0 drivers 声明，driver id "jdbc")
   │  stdio JSON-Lines (JSON-RPC 2.0)
   ▼
dbx-pg83-driver (sidecar)
   ├─ jdbc_url.rs   解析 jdbc:postgresql:// URL（含 url_params、凭据、IPv6、多主机）
   ├─ rewrite83.rs  left()/right() → substring() CASE 改写（词法级，字面量/注释安全）
   ├─ meta83.rs     8.3 兼容元数据 SQL 生成
   └─ pgproto.rs    PG 3.0 线协议（startup/md5/simple query/cancel）
```

smoke 测试（`smoke/smoke.py`）会启动一个实现 3.0 线协议的模拟 PG 8.3 服务器，在字节流层面验证：MD5 认证、`left()`/`right()` 改写真实生效（模拟服务器像真 8.3 一样对原始 `left()` 报 42883）、主键/列信息、函数签名、序列状态、手动事务与错误传递，并全局断言发往服务器的 SQL 不含任何 8.4+ 函数。

## 许可

Apache-2.0（与 DBX 主项目一致）。
