//! 命令行分词：引号感知 + `$VAR` 文本展开。
//!
//! 语义（贴近 POSIX shell）：
//! - 空白（` ` / `\t`）分词；
//! - 双引号 `"..."`：内部展开 `$VAR` 并去除引号，整体作为一个词；
//! - 单引号 `'...'`：字面量，**不**展开 `$VAR`，整体作为一个词；
//! - 引号可在词内拼接，如 `a"b"c` → `abc`；
//! - 未加引号的 `$VAR` 仍展开，其值若含空白会参与下一次分词。

use crate::env::env_render;

/// 单条命令最多参数词数（含命令名）。
pub(crate) const MAX_WORDS: usize = 16;
/// 单个词的最大字节数（与环境变量 VALUE 上限对齐）。
pub(crate) const WORD_CAP: usize = 64;

/// 把一行拆成词（尊重引号），并对每个词做 `$VAR` 展开。
///
/// 词内容写入 `wbuf`（每项 `WORD_CAP` 字节）、长度写入 `wlen`，返回词数。
/// 变量值经 `env_render` 取静态切片，缺失则该 `$VAR` 替换为空。
pub(crate) fn tokenize_line(
    line: &[u8],
    wbuf: &mut [[u8; WORD_CAP]; MAX_WORDS],
    wlen: &mut [usize; MAX_WORDS],
) -> usize {
    let mut nwords = 0usize;
    let mut word = [0u8; WORD_CAP];
    let mut wl = 0usize;
    let mut i = 0usize;
    let n = line.len();

    // 把当前正在累积的词提交到 wbuf（词非空且未满）。
    let mut commit = |word: &[u8; WORD_CAP], wl: &mut usize, wbuf: &mut [[u8; WORD_CAP]; MAX_WORDS], wlen: &mut [usize; MAX_WORDS], nwords: &mut usize| {
        if *wl > 0 && *nwords < MAX_WORDS {
            let l = (*wl).min(WORD_CAP);
            wbuf[*nwords][..l].copy_from_slice(&word[..l]);
            wlen[*nwords] = l;
            *nwords += 1;
        }
        *wl = 0;
    };

    // 把一个 `$VAR` 标识符（已定位 `[s, e)`）的文本形态追加到当前词。
    let mut expand_at = |line: &[u8], s: usize, e: usize, word: &mut [u8; WORD_CAP], wl: &mut usize| {
        let mut tmp = [0u8; 64];
        let n = env_render(&line[s..e], &mut tmp);
        for k in 0..n {
            if *wl < WORD_CAP {
                word[*wl] = tmp[k];
                *wl += 1;
            }
        }
    };

    while i < n {
        let c = line[i];
        if c == b' ' || c == b'\t' {
            commit(&word, &mut wl, wbuf, wlen, &mut nwords);
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            let do_expand = q == b'"'; // 单引号内不展开
            if wl < WORD_CAP {
                word[wl] = q; // 保留起始引号字符
                wl += 1;
            }
            i += 1;
            while i < n && line[i] != q {
                let ch = line[i];
                if do_expand && ch == b'$' && i + 1 < n {
                    let nxt = line[i + 1];
                    if nxt.is_ascii_alphanumeric() || nxt == b'_' {
                        let s = i + 1;
                        let mut e = s;
                        while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                            e += 1;
                        }
                        expand_at(line, s, e, &mut word, &mut wl);
                        i = e;
                        continue;
                    }
                }
                if wl < WORD_CAP {
                    word[wl] = ch;
                    wl += 1;
                }
                i += 1;
            }
            if i < n {
                if wl < WORD_CAP {
                    word[wl] = q; // 保留闭合引号字符
                    wl += 1;
                }
                i += 1; // 跳过闭合引号
            }
            continue;
        }
        // 未加引号的普通字符（含裸 `$VAR`）。
        if c == b'$' && i + 1 < n {
            let nxt = line[i + 1];
            if nxt.is_ascii_alphanumeric() || nxt == b'_' {
                let s = i + 1;
                let mut e = s;
                while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                    e += 1;
                }
                expand_at(line, s, e, &mut word, &mut wl);
                i = e;
                continue;
            }
        }
        if wl < WORD_CAP {
            word[wl] = c;
            wl += 1;
        }
        i += 1;
    }
    commit(&word, &mut wl, wbuf, wlen, &mut nwords);
    nwords
}
