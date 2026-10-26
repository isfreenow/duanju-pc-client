/**
 * 把 package.json 的 version 同步进 src-tauri/tauri.conf.json 与 Cargo.toml
 * （Cargo.lock 由随后的 `cargo update --workspace` 刷新）。
 *
 * CI 每次 push 定版后调用一次；写成独立脚本是为了避开 PowerShell 对引号的
 * 各种转义怪癖——模板字符串放进命令行不如放进文件。
 */
const fs = require("fs");
const path = require("path");

const root = path.join(__dirname, "..");
const version = require(path.join(root, "package.json")).version;
if (!/^\d+\.\d+\.\d+$/.test(version)) {
    console.error(`unexpected version in package.json: ${version}`);
    process.exit(1);
}

const confPath = path.join(root, "src-tauri", "tauri.conf.json");
const conf = JSON.parse(fs.readFileSync(confPath, "utf8"));
conf.version = version;
fs.writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");

const cargoPath = path.join(root, "src-tauri", "Cargo.toml");
const cargo = fs.readFileSync(cargoPath, "utf8");
if (!cargo.includes(`version = "${version}"`)) {
    const next = cargo.replace(/version = "[\d.]+"/, `version = "${version}"`);
    if (next === cargo) {
        console.error("no version field replaced in Cargo.toml");
        process.exit(1);
    }
    fs.writeFileSync(cargoPath, next);
}

console.log(`synced v${version} into tauri.conf.json and Cargo.toml`);
