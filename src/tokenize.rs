//! 命令行分词：引号感知 + `$VAR` 文本展开。

extern crate alloc;

use alloc::vec::Vec;
use crate::env::env_render;

/// 把一行拆成词（尊重引号），并对每个词做 `$VAR` 展开（动态扩容，无长度限制）。
pub(crate) fn tokenize_line(line: &[u8]) -> Vec<Vec<u8>> {
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut word: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let n = line.len();

    let commit = |w: &mut Vec<u8>, words: &mut Vec<Vec<u8>>| {
        if !w.is_empty() {
            words.push(core::mem::take(w));
        }
    };

    let is_var_char = |c: u8| c == b'?' || c.is_ascii_alphanumeric() || c == b'_';

    let expand_at = |line: &[u8], s: usize, e: usize, word: &mut Vec<u8>| {
        let mut tmp = [0u8; 64];
        let n = env_render(&line[s..e], &mut tmp);
        word.extend_from_slice(&tmp[..n]);
    };

    while i < n {
        let c = line[i];
        if c == b' ' || c == b'\t' {
            commit(&mut word, &mut words);
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            let do_expand = q == b'"';
            word.push(q);
            i += 1;
            while i < n && line[i] != q {
                let ch = line[i];
                if do_expand && ch == b'$' && i + 1 < n {
                    let nxt = line[i + 1];
                    if nxt == b'?' {
                        expand_at(line, i + 1, i + 2, &mut word);
                        i += 2;
                        continue;
                    } else if is_var_char(nxt) && nxt != b'?' {
                        let s = i + 1;
                        let mut e = s;
                        while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                            e += 1;
                        }
                        expand_at(line, s, e, &mut word);
                        i = e;
                        continue;
                    }
                }
                word.push(ch);
                i += 1;
            }
            if i < n {
                word.push(q);
                i += 1;
            }
            continue;
        }
        if c == b'$' && i + 1 < n {
            let nxt = line[i + 1];
            if nxt == b'?' {
                expand_at(line, i + 1, i + 2, &mut word);
                i += 2;
                continue;
            } else if is_var_char(nxt) && nxt != b'?' {
                let s = i + 1;
                let mut e = s;
                while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                    e += 1;
                }
                expand_at(line, s, e, &mut word);
                i = e;
                continue;
            }
        }
        word.push(c);
        i += 1;
    }
    commit(&mut word, &mut words);
    words
}
