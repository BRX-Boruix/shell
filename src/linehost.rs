//! \u884c\u7f16\u8f91\u5bbf\u4e3b\u9002\u914d\uff1a\u628a shell \u7684\u7ec8\u7aef\u8f93\u51fa\u3001\u63d0\u793a\u7b26\u4e0e\u8865\u5168\u5019\u9009\u63a5\u7ed9 `libline`\u3002
//!
//! # \u4e3a\u4f55\u9700\u8981\u8fd9\u4e00\u5c42\uff08L-2\uff09
//!
//! `libline` \u53ea\u505a\u7f16\u8f91\u8bed\u4e49\uff0c\u4e0d\u77e5\u9053\u7ec8\u7aef\u662f\u4ec0\u4e48\u3001\u63d0\u793a\u7b26\u957f\u4ec0\u4e48\u6837\u3001
//! \u6709\u54ea\u4e9b\u547d\u4ee4\u53ef\u8865\u5168\u3002\u8fd9\u4e9b\u5168\u662f**\u8c03\u7528\u65b9\u7684\u4e1a\u52a1**\uff0c\u6545\u7531\u672c\u6a21\u5757\u63d0\u4f9b\u3002
//!
//! \u8fd9\u4e5f\u662f L-1 \u90a3\u4e2a\u5c42\u6b21\u5212\u5206\u7684\u5151\u73b0\uff1a\u7f16\u8f91\u8bed\u4e49\uff08\u53ef\u5bbf\u4e3b\u6d4b\uff09\u5728\u5e93\u91cc\uff0c
//! \u6e32\u67d3\uff08\u9700\u7ec8\u7aef\uff09\u5728\u8fd9\u91cc\u3002

use alloc::vec::Vec;

use crate::commands::command_names;
use crate::env::for_each_env_name;
use crate::util::{out, prompt};
use libline::{EditorHost, smart_case_match};

/// shell \u7684 `EditorHost` \u5b9e\u73b0\u3002
pub(crate) struct ShellHost;

impl EditorHost for ShellHost {
    fn write(&mut self, bytes: &[u8]) {
        out(bytes);
    }

    /// \u91cd\u7ed8\uff1a\u56de\u884c\u9996 \u2192 \u6e05\u884c \u2192 \u91cd\u6253\u63d0\u793a\u7b26\u4e0e\u7f13\u51b2\u533a \u2192 \u5149\u6807\u5de6\u79fb\u81f3\u76ee\u6807\u4f4d\u3002
    ///
    /// **\u4e0e\u65e7\u5185\u8054\u5b9e\u73b0\u9010\u5b57\u8282\u4e00\u81f4**\uff08L-2 \u7684\u7ea6\u675f\uff1a\u4ea4\u4e92\u4e0d\u56de\u5f52\uff09\u3002
    fn redraw(&mut self, buffer: &[u8], cursor: usize) {
        out(b"\r");
        out(b"\x1b[K");
        prompt();
        out(buffer);
        let back = buffer.len().saturating_sub(cursor);
        if back > 0 {
            let mut b = [0u8; 24];
            out(b"\x1b[");
            out(crate::util::u64_to_dec(back as u64, &mut b));
            out(b"D");
        }
    }

    /// \u8865\u5168\u5019\u9009\uff1a\u547d\u4ee4\u4f4d\u53d6\u5185\u5efa\u540d\uff0c\u5176\u4f59\u4f4d\u7f6e\u53d6\u73af\u5883\u53d8\u91cf\u540d\u3002
    /// \u5339\u914d\u7528 `libline::smart_case_match`\uff08\u4e0e\u65e7\u884c\u4e3a\u4e00\u81f4\uff09\u3002
    fn completions(&mut self, word: &[u8], is_command: bool) -> Vec<Vec<u8>> {
        let mut matches: Vec<Vec<u8>> = Vec::new();
        if is_command {
            for name in command_names() {
                if smart_case_match(word, name) {
                    matches.push(name.to_vec());
                }
            }
        } else {
            for_each_env_name(|name| {
                if smart_case_match(word, name) {
                    matches.push(name.to_vec());
                }
            });
        }
        matches
    }
}