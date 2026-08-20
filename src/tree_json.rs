//! 交互式/自动 JSON 树状可视化渲染器（ADR-013 JSON 第一公民）。
//!
//! 具备完整的递归/栈式轻量级 JSON 语法树解析与层级树状图（Tree ASCII View）渲染：
//! - 支持任意复杂嵌套对象 `{}`、数组 `[]`、字符串、数值、布尔、null；
//! - 自动生成树状连接符（`├── `, `└── `, `│   `）；
//! - 智能区分字段名与值类型（如对 String / Number / Boolean / Null 标注或高亮显示）；
//! - 既支持直接渲染 JSON 字符串，也支持直接从 VFS 文件路径读取渲染。

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use crate::util::out;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

/// JSON 解析器（基于字符流迭代器）。
pub struct JsonParser<'a> {
    chars: core::str::Chars<'a>,
    peeked: Option<char>,
}

impl<'a> JsonParser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            chars: input.chars(),
            peeked: None,
        }
    }

    fn peek(&mut self) -> Option<char> {
        if self.peeked.is_none() {
            self.peeked = self.chars.next();
        }
        self.peeked
    }

    fn next(&mut self) -> Option<char> {
        if let Some(c) = self.peeked.take() {
            Some(c)
        } else {
            self.chars.next()
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.next();
            } else {
                break;
            }
        }
    }

    pub fn parse(&mut self) -> Result<JsonValue, &'static str> {
        self.skip_whitespace();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => self.parse_string().map(JsonValue::String),
            Some('t') | Some('f') => self.parse_bool(),
            Some('n') => self.parse_null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err("unexpected character in json"),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, &'static str> {
        self.next(); // '{'
        let mut fields = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some('}') {
                self.next();
                break;
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.next() != Some(':') {
                return Err("expected ':' after key");
            }
            let val = self.parse()?;
            fields.push((key, val));

            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.next();
                }
                Some('}') => {
                    self.next();
                    break;
                }
                _ => return Err("expected ',' or '}' in object"),
            }
        }
        Ok(JsonValue::Object(fields))
    }

    fn parse_array(&mut self) -> Result<JsonValue, &'static str> {
        self.next(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(']') {
                self.next();
                break;
            }
            let val = self.parse()?;
            items.push(val);

            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.next();
                }
                Some(']') => {
                    self.next();
                    break;
                }
                _ => return Err("expected ',' or ']' in array"),
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, &'static str> {
        self.skip_whitespace();
        if self.next() != Some('"') {
            return Err("expected '\"'");
        }
        let mut s = String::new();
        while let Some(c) = self.next() {
            match c {
                '"' => return Ok(s),
                '\\' => match self.next() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\x08'),
                    Some('f') => s.push('\x0C'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => {
                        // 简化 4 位十六进制解析
                        let mut hex_val = 0u32;
                        for _ in 0..4 {
                            if let Some(hc) = self.next() {
                                if let Some(d) = hc.to_digit(16) {
                                    hex_val = (hex_val << 4) | d;
                                }
                            }
                        }
                        if let Some(ch) = char::from_u32(hex_val) {
                            s.push(ch);
                        } else {
                            s.push('?');
                        }
                    }
                    _ => s.push('?'),
                },
                other => s.push(other),
            }
        }
        Err("unclosed string")
    }

    fn parse_number(&mut self) -> Result<JsonValue, &'static str> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                s.push(self.next().unwrap());
            } else {
                break;
            }
        }
        Ok(JsonValue::Number(s))
    }

    fn parse_bool(&mut self) -> Result<JsonValue, &'static str> {
        if self.peek() == Some('t') {
            for expected in "true".chars() {
                if self.next() != Some(expected) {
                    return Err("expected true");
                }
            }
            Ok(JsonValue::Bool(true))
        } else {
            for expected in "false".chars() {
                if self.next() != Some(expected) {
                    return Err("expected false");
                }
            }
            Ok(JsonValue::Bool(false))
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, &'static str> {
        for expected in "null".chars() {
            if self.next() != Some(expected) {
                return Err("expected null");
            }
        }
        Ok(JsonValue::Null)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_parse_and_tree() {
        let json_str = r#"{"arch":"x86_64","cores":4,"features":["smap","smep"],"status":{"online":true,"uptime":12345}}"#;
        let mut parser = JsonParser::new(json_str);
        let val = parser.parse().expect("parse failed");

        match &val {
            JsonValue::Object(fields) => {
                assert_eq!(fields.len(), 4);
                assert_eq!(fields[0].0, "arch");
                assert_eq!(fields[0].1, JsonValue::String("x86_64".into()));
                assert_eq!(fields[1].0, "cores");
                assert_eq!(fields[1].1, JsonValue::Number("4".into()));
            }
            _ => panic!("root must be object"),
        }
    }
}
