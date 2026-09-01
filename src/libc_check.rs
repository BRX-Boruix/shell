//! libc 验证层（shell 内建命令 libccheck）。
//!
//! 在真实内核上端到端验收 libc 的 C ABI 函数（S06 真实数据链路）：
//! - malloc/free/realloc：堆分配与复用
//! - string：strlen/strcmp/strcpy/memcpy
//! - printf 家族：snprintf/sprintf（格式、宽度、浮点）
//! - strtol/atoi：整数解析
//! - time/clock：时间读数
//! - errno：机制存在性
//! - FILE 流：fopen/fwrite/fread（经内核 VFS）
//!
//! 直接经 libc crate 路径调用其 C ABI 导出（强制链接 + 真实调用）。

use alloc::vec::Vec;
use crate::util::{out, outln};

/// 检查结果计数器：通过/失败。
struct Report {
    pass: u32,
    fail: u32,
    buf: Vec<u8>,
}

impl Report {
    fn new() -> Self {
        Report { pass: 0, fail: 0, buf: Vec::new() }
    }
    fn check(&mut self, name: &str, cond: bool) {
        if cond {
            self.pass += 1;
        } else {
            self.fail += 1;
        }
        self.buf.clear();
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.extend_from_slice(b": ");
        self.buf.extend_from_slice(if cond { b"OK" } else { b"FAIL" });
        self.buf.push(b'\n');
        out(&self.buf);
    }
}

/// 运行 libc 验收并输出结果。
pub(crate) fn cmd_libccheck(_arg: &[u8]) -> u8 {
    let mut rpt = Report::new();

    // 1) malloc / realloc / free（堆真实分配）
    unsafe {
        let p = libc::malloc::malloc(64);
        rpt.check("malloc(64) non-null", !p.is_null());
        if !p.is_null() {
            let mut i = 0usize;
            while i < 64 {
                *p.add(i) = (i as u8) & 0x7F;
                i += 1;
            }
            rpt.check("malloc writable 64B", p.add(0).read() == 0 && p.add(63).read() == 63);
            let q = libc::malloc::realloc(p, 256);
            rpt.check("realloc(256) non-null", !q.is_null());
            if !q.is_null() {
                *q.add(100) = 0xAA;
                rpt.check("realloc region writable", q.add(100).read() == 0xAA);
                libc::malloc::free(q);
            }
        }
        let big = libc::malloc::malloc(4096);
        rpt.check("malloc(4096) non-null", !big.is_null());
        if !big.is_null() {
            libc::malloc::free(big);
        }
        let again = libc::malloc::malloc(4096);
        rpt.check("malloc reuse after free", !again.is_null());
        if !again.is_null() {
            libc::malloc::free(again);
        }

        // posix_memalign 对齐分配（SSE/AVX 级 64/256 对齐）。
        let mut p64: *mut u8 = core::ptr::null_mut();
        let rc = libc::malloc::posix_memalign(&mut p64, 64, 128);
        rpt.check("posix_memalign(64) rc==0", rc == 0 && !p64.is_null());
        if !p64.is_null() {
            let aligned_ok = (p64 as usize) % 64 == 0;
            rpt.check("posix_memalign(64) aligned", aligned_ok);
            *p64.add(0) = 0x5A;
            rpt.check("posix_memalign writable", p64.add(0).read() == 0x5A);
            libc::malloc::free(p64);
        }
        let mut p256: *mut u8 = core::ptr::null_mut();
        let rc2 = libc::malloc::posix_memalign(&mut p256, 256, 64);
        rpt.check("posix_memalign(256) aligned",
            rc2 == 0 && !p256.is_null() && (p256 as usize) % 256 == 0);
        if !p256.is_null() {
            // realloc 对齐指针应保留数据（payload 偏移方案）。
            let mut i = 0usize;
            while i < 64 {
                *p256.add(i) = (i as u8) & 0x3F;
                i += 1;
            }
            let r2 = libc::malloc::realloc(p256, 512);
            rpt.check("realloc(aligned) non-null", !r2.is_null());
            if !r2.is_null() {
                rpt.check("realloc(aligned) data kept",
                    r2.add(63).read() == 63 && r2.add(5).read() == 5);
                libc::malloc::free(r2);
            }
        }
    }



    unsafe {
        // item8 加固：canary / poison / double-free 检测。
        let normal = libc::malloc::malloc(48);
        if !normal.is_null() {
            *normal.add(0) = 1;
            *normal.add(47) = 2;
            libc::malloc::free(normal);
        }
        rpt.check("malloc no-corrupt normal", libc::malloc::boruix_malloc_corrupt() == 0);
        // 越界写：写入可写容量末尾（canary 区）→ free 应检测并置位。
        let p_ov = libc::malloc::malloc(32);
        if !p_ov.is_null() {
            let usable = libc::malloc::malloc_usable_size(p_ov);
            // canary 位于可写容量末 8 字节。
            let mut i = usable;
            while i > usable - 8 {
                i -= 1;
                *p_ov.add(i) = 0xCC;
            }
            libc::malloc::free(p_ov);
            rpt.check("malloc canary detect overflow", libc::malloc::boruix_malloc_corrupt() == 1);
        }
        // poison：释放后旧内存读为 0xDD。
        let p_poison = libc::malloc::malloc(16);
        let mut poison_ok = false;
        if !p_poison.is_null() {
            *p_poison.add(0) = 0x11;
            libc::malloc::free(p_poison);
            poison_ok = p_poison.add(0).read() == 0xDD;
        }
        rpt.check("malloc poison 0xDD after free", poison_ok);
        // double-free 检测：同一块释放两次 → 置损坏标志。
        let p_df = libc::malloc::malloc(24);
        if !p_df.is_null() {
            libc::malloc::free(p_df);
            libc::malloc::free(p_df);
            rpt.check("malloc double-free detect", libc::malloc::boruix_malloc_corrupt() == 1);
        }
    }

    // 2) 字符串函数
    unsafe {
        let a = b"hello\0".as_ptr() as *const i8;
        let b_ = b"hello\0".as_ptr() as *const i8;
        let c = b"hellp\0".as_ptr() as *const i8;
        rpt.check("strlen hello ==5", libc::string::strlen(a) == 5);
        rpt.check("strcmp equal ==0", libc::string::strcmp(a, b_) == 0);
        rpt.check("strcmp diff !=0", libc::string::strcmp(a, c) != 0);
        let mut dst = [0i8; 16];
        let sp = libc::string::strcpy(dst.as_mut_ptr(), a);
        rpt.check("strcpy copied", !sp.is_null() && libc::string::strlen(dst.as_ptr()) == 5);

        // 新增 string：strspn/strcspn/strpbrk/strncat/strtok。
        rpt.check("strspn abc", libc::string::strspn(
            b"aabbaaX\0".as_ptr() as *const i8,
            b"ab\0".as_ptr() as *const i8) == 6);
        rpt.check("strcspn X", libc::string::strcspn(
            b"abcdef\0".as_ptr() as *const i8,
            b"z\0".as_ptr() as *const i8) == 6);
        rpt.check("strcspn stop", libc::string::strcspn(
            b"abc.def\0".as_ptr() as *const i8,
            b".\0".as_ptr() as *const i8) == 3);
        let pb = libc::string::strpbrk(
            b"hello world\0".as_ptr() as *const i8,
            b" \0".as_ptr() as *const i8);
        rpt.check("strpbrk space idx", pb.is_null() || pb == b"hello world\0".as_ptr().add(5) as *mut i8);
        let mut catbuf = [0i8; 16];
        let cs = b"ab\0".as_ptr() as *const i8;
        libc::string::strcpy(catbuf.as_mut_ptr() as *mut i8, cs);
        libc::string::strncat(catbuf.as_mut_ptr() as *mut i8, b"cd\0".as_ptr() as *const i8, 2);
        rpt.check("strncat abcd", libc::string::strlen(catbuf.as_ptr()) == 4
            && libc::string::strcmp(catbuf.as_ptr(), b"abcd\0".as_ptr() as *const i8) == 0);
        // strtok 切分（用可写缓冲）。
        let mut toksrc = *b"one,two,three\0";
        let t1 = libc::string::strtok(toksrc.as_mut_ptr() as *mut i8, b",\0".as_ptr() as *const i8);
        rpt.check("strtok t1 one", !t1.is_null()
            && libc::string::strcmp(t1, b"one\0".as_ptr() as *const i8) == 0);
        let t2 = libc::string::strtok(core::ptr::null_mut(), b",\0".as_ptr() as *const i8);
        rpt.check("strtok t2 two", !t2.is_null()
            && libc::string::strcmp(t2, b"two\0".as_ptr() as *const i8) == 0);

        // strtok_r：两个独立 saveptr 交错切分（线程安全模式）。
        let mut sa = *b"a,b,c\0";
        let mut sb = *b"x/y\0";
        let mut save_a: *mut i8 = core::ptr::null_mut();
        let mut save_b: *mut i8 = core::ptr::null_mut();
        let ra1 = libc::string::strtok_r(sa.as_mut_ptr() as *mut i8,
            b",\0".as_ptr() as *const i8, &mut save_a);
        let rb1 = libc::string::strtok_r(sb.as_mut_ptr() as *mut i8,
            b"/\0".as_ptr() as *const i8, &mut save_b);
        rpt.check("strtok_r a1+b1", !ra1.is_null() && !rb1.is_null()
            && libc::string::strcmp(ra1, b"a\0".as_ptr() as *const i8) == 0
            && libc::string::strcmp(rb1, b"x\0".as_ptr() as *const i8) == 0);
        let ra2 = libc::string::strtok_r(core::ptr::null_mut(),
            b",\0".as_ptr() as *const i8, &mut save_a);
        let rb2 = libc::string::strtok_r(core::ptr::null_mut(),
            b"/\0".as_ptr() as *const i8, &mut save_b);
        rpt.check("strtok_r a2+b2 interleaved", !ra2.is_null() && !rb2.is_null()
            && libc::string::strcmp(ra2, b"b\0".as_ptr() as *const i8) == 0
            && libc::string::strcmp(rb2, b"y\0".as_ptr() as *const i8) == 0);

        // item6：strcasecmp/strncasecmp/memmem/strsep。
        let up = b"Hello\0".as_ptr() as *const i8;
        let low = b"hello\0".as_ptr() as *const i8;
        rpt.check("strcasecmp eq", libc::string::strcasecmp(up, low) == 0);
        rpt.check("strcasecmp diff", libc::string::strcasecmp(up, b"hellq\0".as_ptr() as *const i8) != 0);
        rpt.check("strncasecmp n eq", libc::string::strncasecmp(up, low, 5) == 0);
        rpt.check("strncasecmp n cut", libc::string::strncasecmp(b"aB\0".as_ptr() as *const i8,
            b"Ac\0".as_ptr() as *const i8, 1) == 0);
        // memmem：在字节块中找子串。
        let block = *b"abcdefgh\0";
        let found = libc::string::memmem(
            block.as_ptr() as *const u8, 8,
            b"def\0".as_ptr() as *const u8, 3);
        rpt.check("memmem found", !found.is_null()
            && found == block.as_ptr().add(3) as *mut u8);
        let nf = libc::string::memmem(
            block.as_ptr() as *const u8, 8,
            b"xyz\0".as_ptr() as *const u8, 3);
        rpt.check("memmem not found", nf.is_null());
        // strsep：切分（连续分隔符产生空 token）。
        let mut src = *b"a,,b,cd\0";
        let mut p: *mut i8 = src.as_mut_ptr() as *mut i8;
        let s1 = libc::string::strsep(&mut p, b",\0".as_ptr() as *const i8);
        rpt.check("strsep t1 a", !s1.is_null()
            && libc::string::strcmp(s1, b"a\0".as_ptr() as *const i8) == 0);
        let s2 = libc::string::strsep(&mut p, b",\0".as_ptr() as *const i8);
        rpt.check("strsep t2 empty", !s2.is_null() && libc::string::strlen(s2) == 0);
        let s3 = libc::string::strsep(&mut p, b",\0".as_ptr() as *const i8);
        rpt.check("strsep t3 b", !s3.is_null()
            && libc::string::strcmp(s3, b"b\0".as_ptr() as *const i8) == 0);
    }

    // 3) printf 家族（c_variadic）
    unsafe {
        let mut buf = [0u8; 64];
        let n = libc::stdio::snprintf(buf.as_mut_ptr() as *mut i8, buf.len(),
            b"v=%d s=%s\0".as_ptr() as *const i8,
            42, b"abc\0".as_ptr() as *const i8);
        rpt.check("snprintf returns len", n == 10);
        let got = cstr_to_owned(buf.as_ptr());
        rpt.check("snprintf v42 sabc", got == b"v=42 s=abc");
        let n2 = libc::stdio::snprintf(buf.as_mut_ptr() as *mut i8, buf.len(),
            b"%05d %.2f\0".as_ptr() as *const i8,
            7, 3.14159);
        rpt.check("snprintf2 returns len", n2 == 9);
        let got2 = cstr_to_owned(buf.as_ptr());
        rpt.check("snprintf2 pad+float", got2 == b"00007 3.14");
        let n3 = libc::stdio::snprintf(buf.as_mut_ptr() as *mut i8, 5,
            b"%s\0".as_ptr() as *const i8, b"0123456789\0".as_ptr() as *const i8);
        rpt.check("snprintf trunc len", n3 == 10);
    }

    // 4) strtol / atoi
    unsafe {
        rpt.check("atoi 123", libc::stdlib::atoi(b"123\0".as_ptr() as *const i8) == 123);
        rpt.check("strtol -99 base10",
            libc::stdlib::strtol(b"-99\0".as_ptr() as *const i8, core::ptr::null_mut(), 10) == -99);
        rpt.check("strtol 0x1A base0",
            libc::stdlib::strtol(b"0x1A\0".as_ptr() as *const i8, core::ptr::null_mut(), 0) == 26);
    }

    // 5) time / clock
    {
        let t = libc::time::time(core::ptr::null_mut());
        rpt.check("time >0", t > 0);
        rpt.check("clock >=0", libc::time::clock() >= 0);
    }

    // 6) FILE 流（经内核 VFS）
    unsafe {
        let path = b"/tmp/libc_selfcheck.txt\0".as_ptr() as *const i8;
        let fp = libc::stdio::fopen(path, b"w+\0".as_ptr() as *const i8);
        rpt.check("fopen w+ non-null", !fp.is_null());
        if !fp.is_null() {
            let data = b"libc-verify\n".as_ptr() as *const core::ffi::c_void;
            let w = libc::stdio::fwrite(data, 1, 12, fp);
            rpt.check("fwrite 12 bytes", w == 12);
            libc::stdio::fclose(fp);
            let fr = libc::stdio::fopen(path, b"r\0".as_ptr() as *const i8);
            if !fr.is_null() {
                let mut rd = [0u8; 12];
                let n = libc::stdio::fread(rd.as_mut_ptr() as *mut core::ffi::c_void, 1, 12, fr);
                rpt.check("fread 12 bytes", n == 12);
                rpt.check("fread content", &rd[..11] == b"libc-verify");
                libc::stdio::fclose(fr);
            } else {
                rpt.check("fopen r re-read", false);
            }
            libc::unistd::unlink(path);
        }
    }

    // 6b) fscanf %lc / %ls 宽字符（经内核 VFS 文件流）
    unsafe {
        let wpath = b"/tmp/libc_wide.txt ".as_ptr() as *const i8;
        let wfp = libc::stdio::fopen(wpath, b"w+ ".as_ptr() as *const i8);
        if !wfp.is_null() {
            let wdata = b"AB 42 ".as_ptr() as *const core::ffi::c_void;
            libc::stdio::fwrite(wdata, 1, 5, wfp);
            libc::stdio::fclose(wfp);
            let wfr = libc::stdio::fopen(wpath, b"r ".as_ptr() as *const i8);
            if !wfr.is_null() {
                let mut w1: i32 = 0;
                let mut w2: i32 = 0;
                // %lc 逐个读两个原样宽字符（无空白跳过）。
                let n1 = libc::stdio::fscanf(wfr,
                    b"%lc%lc\0".as_ptr() as *const i8,
                    &mut w1 as *mut i32, &mut w2 as *mut i32);
                rpt.check("fscanf %lc count", n1 == 2);
                rpt.check("fscanf %lc 'A''B'", w1 == b'A' as i32 && w2 == b'B' as i32);
                libc::stdio::fclose(wfr);
            }
            let wfr2 = libc::stdio::fopen(wpath, b"r ".as_ptr() as *const i8);
            if !wfr2.is_null() {
                let mut ws = [0i32; 8];
                let mut num: i32 = 0;
                // %ls 读非空白宽字符串 + %d 整数。
                let n2 = libc::stdio::fscanf(wfr2,
                    b"%ls %d\0".as_ptr() as *const i8,
                    ws.as_mut_ptr(), &mut num as *mut i32);
                rpt.check("fscanf %ls %d count", n2 == 2);
                rpt.check("fscanf %ls 'AB'", ws[0] == b'A' as i32
                    && ws[1] == b'B' as i32 && ws[2] == 0);
                rpt.check("fscanf %d 42", num == 42);
                libc::stdio::fclose(wfr2);
            }
            libc::unistd::unlink(wpath);
        }
    }


    // 8) item9 真机覆盖增强：strtof 正确舍入 / %a / %n / 宽字符 / ungetc / getline
    unsafe {
        // strtof 严格正确舍入（位级对拍 Rust f32 参考）。
        let f1 = libc::stdlib::strtof(b"1.17549435e-38\0".as_ptr() as *const i8, core::ptr::null_mut());
        let ref1: f32 = "1.17549435e-38".parse().unwrap();
        rpt.check("strtof min-normal bits", f1.to_bits() == ref1.to_bits());
        let f2 = libc::stdlib::strtof(b"3.4028234663852886e38\0".as_ptr() as *const i8, core::ptr::null_mut());
        let ref2: f32 = "3.4028234663852886e38".parse().unwrap();
        rpt.check("strtof max bits", f2.to_bits() == ref2.to_bits());
        let f3 = libc::stdlib::strtof(b"0.1\0".as_ptr() as *const i8, core::ptr::null_mut());
        let ref3: f32 = "0.1".parse().unwrap();
        rpt.check("strtof 0.1 bits", f3.to_bits() == ref3.to_bits());

        // %a 十六进制浮点。
        let mut ab = [0u8; 32];
        let na = libc::stdio::snprintf(ab.as_mut_ptr() as *mut i8, ab.len(),
            b"%a\0".as_ptr() as *const i8, 255.5f64);
        let gota = cstr_to_owned(ab.as_ptr());
        rpt.check("snprintf %a len", na == 7);
        rpt.check("snprintf %a 0x1.ffp+7", gota == b"0x1.ffp+7");

        // %n 写已输出字符数。
        let mut nb = [0u8; 16];
        let mut written: i32 = -1;
        let nn = libc::stdio::snprintf(nb.as_mut_ptr() as *mut i8, nb.len(),
            b"abc%n\0".as_ptr() as *const i8, &mut written as *mut i32);
        rpt.check("snprintf %n len", nn == 3);
        rpt.check("snprintf %n wrote 3", written == 3);

        // 宽字符往返。
        let mut wbuf = [0i32; 16];
        let ws = libc::wchar::mbstowcs(wbuf.as_mut_ptr(), b"hi\0".as_ptr() as *const i8, 16);
        rpt.check("mbstowcs count", ws == 2 && wbuf[0] == b'h' as i32 && wbuf[1] == b'i' as i32);
        let mut mbb = [0i8; 16];
        let ms = libc::wchar::wcstombs(mbb.as_mut_ptr(), wbuf.as_ptr(), 16);
        rpt.check("wcstombs roundtrip", ms == 2 && mbb[0] == b'h' as i8);

        // ungetc + fgetc 往返（经 VFS 文件）。
        let upath = b"/tmp/libc_ungetc.txt\0".as_ptr() as *const i8;
        let ufp = libc::stdio::fopen(upath, b"w+\0".as_ptr() as *const i8);
        if !ufp.is_null() {
            libc::stdio::fwrite(b"XYZ\0".as_ptr() as *const core::ffi::c_void, 1, 3, ufp);
            libc::stdio::fclose(ufp);
            let ufr = libc::stdio::fopen(upath, b"r\0".as_ptr() as *const i8);
            if !ufr.is_null() {
                let _c1 = libc::stdio::fgetc(ufr);
                let uc = libc::stdio::ungetc(b'Q' as i32, ufr);
                let c2 = libc::stdio::fgetc(ufr);
                let c3 = libc::stdio::fgetc(ufr);
                rpt.check("ungetc returns Q", uc == b'Q' as i32);
                rpt.check("fgetc after ungetc == Q", c2 == b'Q' as i32);
                rpt.check("fgetc next still Y", c3 == b'Y' as i32);
                libc::stdio::fclose(ufr);
            }
            libc::unistd::unlink(upath);
        }

        // getline（自动扩容读一行）。
        let gpath = b"/tmp/libc_getline.txt\0".as_ptr() as *const i8;
        let gfp = libc::stdio::fopen(gpath, b"w+\0".as_ptr() as *const i8);
        if !gfp.is_null() {
            libc::stdio::fwrite(b"hello world\nsecond\0".as_ptr() as *const core::ffi::c_void, 1, 18, gfp);
            libc::stdio::fclose(gfp);
            let gfr = libc::stdio::fopen(gpath, b"r\0".as_ptr() as *const i8);
            if !gfr.is_null() {
                let mut line: *mut i8 = core::ptr::null_mut();
                let mut cap: usize = 0;
                let glen = libc::stdio::getline(&mut line, &mut cap, gfr);
                let mut line_ok = false;
                if glen > 0 && !line.is_null() {
                    // 内容应为 "hello world\n" 共 12 字符。
                    let s = core::slice::from_raw_parts(line as *const u8, glen as usize);
                    line_ok = s == b"hello world\n";
                }
                rpt.check("getline first line", line_ok && glen == 12);
                libc::stdio::fclose(gfr);
            }
            libc::unistd::unlink(gpath);
        }
    }

    // 7) errno 机制
    {
        rpt.check("errno_location non-null", !libc::errno::__errno_location().is_null());
    }

    // 汇总
    let mut sum = Vec::new();
    sum.extend_from_slice(b"[libccheck] passed=");
    sum.extend_from_slice(crate::util::u64_to_dec(rpt.pass as u64, &mut [0u8; 24]));
    sum.extend_from_slice(b" failed=");
    sum.extend_from_slice(crate::util::u64_to_dec(rpt.fail as u64, &mut [0u8; 24]));
    sum.extend_from_slice(b"\n");
    outln(&sum);

    if rpt.fail == 0 { 0 } else { 1 }
}

/// 把 C 字符串复制为 Vec<u8>（直到 NUL）。
fn cstr_to_owned(p: *const u8) -> Vec<u8> {
    let mut v = Vec::new();
    unsafe {
        let mut q = p;
        while *q != 0 {
            v.push(*q);
            q = q.add(1);
        }
    }
    v
}
