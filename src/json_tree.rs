//! JSON 语法树状可视化渲染（Tree ASCII View）。
//!
//! 纯呈现层：只消费 `libsys::json::JsonValue`（解析由 libsys 承担，ADR-024），
//! 本模块负责把解析结果画成层级树（`├──` / `└──` / `│` ASCII 或 UTF-8 盒子绘图）。
//! 与解析职责分离：parser 在 libsys，renderer 留在 shell。

extern crate alloc;

use crate::util::out;
use libsys::json::JsonValue;

/// 将解析好的 JSON 树状结构打印到终端（默认 ASCII 连接符，use_utf8=true 时使用 UTF-8 盒子绘图字符）。
pub fn print_tree(val: &JsonValue, root_name: Option<&str>, use_utf8: bool) {
    let name = root_name.unwrap_or(".");
    out(name.as_bytes());
    out(b"\n");
    print_value(val, "", use_utf8);
}

fn print_value(val: &JsonValue, prefix: &str, use_utf8: bool) {
    match val {
        JsonValue::Object(fields) => {
            let total = fields.len();
            for (idx, (k, v)) in fields.iter().enumerate() {
                let is_last = idx + 1 == total;
                let branch = if use_utf8 {
                    if is_last { "└── " } else { "├── " }
                } else {
                    if is_last { "`-- " } else { "|-- " }
                };
                let next_prefix = if use_utf8 {
                    if is_last {
                        alloc::format!("{}    ", prefix)
                    } else {
                        alloc::format!("{}│   ", prefix)
                    }
                } else {
                    if is_last {
                        alloc::format!("{}    ", prefix)
                    } else {
                        alloc::format!("{}|   ", prefix)
                    }
                };

                match v {
                    JsonValue::Object(_) => {
                        out(alloc::format!("{}{}{}: (object)\n", prefix, branch, k).as_bytes());
                        print_value(v, &next_prefix, use_utf8);
                    }
                    JsonValue::Array(_) => {
                        out(alloc::format!("{}{}{}: (array)\n", prefix, branch, k).as_bytes());
                        print_value(v, &next_prefix, use_utf8);
                    }
                    _ => {
                        out(alloc::format!("{}{}{}: ", prefix, branch, k).as_bytes());
                        print_leaf(v);
                        out(b"\n");
                    }
                }
            }
        }
        JsonValue::Array(items) => {
            let total = items.len();
            for (idx, v) in items.iter().enumerate() {
                let is_last = idx + 1 == total;
                let branch = if use_utf8 {
                    if is_last { "└── " } else { "├── " }
                } else {
                    if is_last { "`-- " } else { "|-- " }
                };
                let next_prefix = if use_utf8 {
                    if is_last {
                        alloc::format!("{}    ", prefix)
                    } else {
                        alloc::format!("{}│   ", prefix)
                    }
                } else {
                    if is_last {
                        alloc::format!("{}    ", prefix)
                    } else {
                        alloc::format!("{}|   ", prefix)
                    }
                };

                match v {
                    JsonValue::Object(_) => {
                        out(alloc::format!("{}{}[{}]: (object)\n", prefix, branch, idx).as_bytes());
                        print_value(v, &next_prefix, use_utf8);
                    }
                    JsonValue::Array(_) => {
                        out(alloc::format!("{}{}[{}]: (array)\n", prefix, branch, idx).as_bytes());
                        print_value(v, &next_prefix, use_utf8);
                    }
                    _ => {
                        out(alloc::format!("{}{}[{}]: ", prefix, branch, idx).as_bytes());
                        print_leaf(v);
                        out(b"\n");
                    }
                }
            }
        }
        _ => {
            out(prefix.as_bytes());
            print_leaf(val);
            out(b"\n");
        }
    }
}

fn print_leaf(val: &JsonValue) {
    match val {
        JsonValue::Null => out(b"null"),
        JsonValue::Bool(b) => out(if *b { b"true" } else { b"false" }),
        JsonValue::Number(n) => out(n.as_bytes()),
        JsonValue::String(s) => {
            out(b"\"");
            out(s.as_bytes());
            out(b"\"");
        }
        _ => {}
    }
}
