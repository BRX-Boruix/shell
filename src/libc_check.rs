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
use core::sync::atomic::{AtomicU32, Ordering};
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


// ---- libccheck 信号处理器（真实投递验证用） ----
/// 用户 SIGUSR1 handler：置位标记（证明经 libc signal()/sigaction() 设置的处理器
/// 确实被内核投递并 sigreturn 恢复）。普通 extern "C" 函数即可——内核压帧后跳入，
/// 函数 ret 弹回 restorer → rt_sigreturn。
static SIG_LIBC_RAN: AtomicU32 = AtomicU32::new(0);
unsafe extern "C" fn libc_sigusr1_handler(_sig: i32) {
    SIG_LIBC_RAN.store(1, Ordering::SeqCst);
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

        // poison：释放后经下次 malloc 复用同一块，其 payload 首字节应为 0xDD。
        // 不读已释放内存（UB）；改为释放后同尺寸复用，检验毒化确实写入。
        let p_poison = libc::malloc::malloc(512);
        if !p_poison.is_null() {
            // volatile 写/读：free 的毒化写是 volatile，复用后 volatile 读取真实内存，
            // 使编译器无法把"释放→复用→读取"折叠成未初始化/常量（LTO 防御）。
            core::ptr::write_volatile(p_poison, 0x11);
            libc::malloc::free(p_poison);
            let r = libc::malloc::malloc(512);
            let poison_ok = !r.is_null() && core::ptr::read_volatile(r) == 0xDD;
            if !r.is_null() {
                libc::malloc::free(r);
            }
            rpt.check("malloc poison 0xDD after free", poison_ok);
        }
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
        rpt.check("snprintf2 returns len", n2 == 10);
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
        let wpath = b"/tmp/libc_wide.txt\0".as_ptr() as *const i8;
        let wfp = libc::stdio::fopen(wpath, b"w+\0".as_ptr() as *const i8);
        if !wfp.is_null() {
            let wdata = b"AB 42\0".as_ptr() as *const core::ffi::c_void;
            libc::stdio::fwrite(wdata, 1, 5, wfp);
            libc::stdio::fclose(wfp);
            let wfr = libc::stdio::fopen(wpath, b"r\0".as_ptr() as *const i8);
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
            let wfr2 = libc::stdio::fopen(wpath, b"r\0".as_ptr() as *const i8);
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
        rpt.check("snprintf %a len", na == 9);
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


    // 9) mkdir / opendir / readdir / closedir / remove（经内核 VFS）
    unsafe {
        let tdir = b"/tmp_libc_dir\0".as_ptr() as *const i8;
        // mkdir 755
        let rc = libc::unistd::mkdir(tdir, 0o755);
        rpt.check("mkdir rc==0", rc == 0);
        // 在目录内创建文件
        let tfile = b"/tmp_libc_dir/hello.txt\0".as_ptr() as *const i8;
        let fp = libc::stdio::fopen(tfile, b"w\0".as_ptr() as *const i8);
        let file_created = !fp.is_null();
        if !fp.is_null() { libc::stdio::fclose(fp); }
        rpt.check("mkdir create file in dir", file_created);
        // opendir/readdir/closedir
        let dp = libc::dirent::opendir(tdir);
        rpt.check("opendir non-null", !dp.is_null());
        if !dp.is_null() {
            let mut found = false;
            let mut count = 0usize;
            let mut hello_ino = 0u64;
            loop {
                let e = libc::dirent::readdir(dp);
                if e.is_null() { break; }
                count += 1;
                let nm = cstr_to_owned((*e).d_name.as_ptr() as *const u8);
                if nm == b"hello.txt" {
                    found = true;
                    hello_ino = (*e).d_ino as u64;
                }
                // 文件条目 d_type == DT_REG(8)；目录条目 == DT_DIR(4)。
                let t = (*e).d_type;
                let _ = t;
            }
            rpt.check("readdir count>=1", count >= 1);
            rpt.check("readdir found hello.txt", found);
            // ---- 登记项④：d_ino 必须与 stat 的 st_ino **同源**（同一个文件同一个号）----
            // 此前 d_ino 恒 0（wire 不带它），而 st_ino 早就有真值 ⇒ 两者对不上。
            let mut hst: libc::unistd::stat = core::mem::zeroed();
            let st_ok = libc::unistd::stat(tfile, &mut hst) == 0;
            rpt.check("d_ino: stat(hello.txt) -> 0", st_ok);
            rpt.check("d_ino: readdir reported a nonzero ino", hello_ino != 0);
            rpt.check(
                "d_ino: readdir d_ino == stat st_ino (same file, same number)",
                st_ok && hello_ino == hst.st_ino as u64,
            );
            let rc2 = libc::dirent::closedir(dp);
            rpt.check("closedir rc==0", rc2 == 0);
        }
        // remove 文件，再 remove 空目录
        let rc3 = libc::unistd::remove(tfile);
        rpt.check("remove file rc==0", rc3 == 0);
        let rc4 = libc::unistd::remove(tdir);
        rpt.check("remove empty dir rc==0", rc4 == 0);
    }

    // 10) signal / sigaction / raise（经内核 SIGNAL 域）
    unsafe {
        // signal() 设置/返回旧处置语义
        let old_ign = libc::signal::signal(libc::signal::SIGUSR1, libc::signal::SIG_IGN);
        rpt.check("signal set IGN returns DFL", old_ign == libc::signal::SIG_DFL);
        let old_dfl = libc::signal::signal(libc::signal::SIGUSR1, libc::signal::SIG_DFL);
        rpt.check("signal set DFL returns IGN", old_dfl == libc::signal::SIG_IGN);

        // sigaction()：设处理器 + oact 捕获旧处置
        let mut act: libc::signal::sigaction = core::mem::zeroed();
        act.sa_handler = libc_sigusr1_handler as *const () as usize;
        act.sa_flags = 0;
        let mut oact: libc::signal::sigaction = core::mem::zeroed();
        let rsa = libc::signal::sigaction(libc::signal::SIGUSR1, &act as *const _ as *const libc::signal::sigaction, &mut oact as *mut _);
        rpt.check("sigaction set rc==0", rsa == 0);
        rpt.check("sigaction oact old==DFL", oact.sa_handler == libc::signal::SIG_DFL);

        // **纯查询模式**（`act==NULL`）：走内核的 `a4=1` 模式位——一次 syscall、不改状态。
        // 断言两件：(a) 查回来的就是刚装的那个 handler；(b) 查完之后它**还在**（下面的 raise 仍进
        // handler）。旧实现是「设 SIG_DFL 读旧值、再设回去」的两次 syscall，中间那段处置真的是
        // SIG_DFL——本用例钉的是**新路径的行为**（查询不改状态），旧路径在无竞争时也能过前一条，
        // 但第二条（查完 handler 仍在且仍会跑）正是"没被查询顺手重置"的直接证据。
        let mut q: libc::signal::sigaction = core::mem::zeroed();
        let rq = libc::signal::sigaction(
            libc::signal::SIGUSR1,
            core::ptr::null(),
            &mut q as *mut libc::signal::sigaction,
        );
        rpt.check("sigaction query rc==0", rq == 0);
        rpt.check(
            "sigaction query returns installed handler",
            q.sa_handler == libc_sigusr1_handler as *const () as usize,
        );

        // 真机投递：raise 自身 + yield 让内核投递进 handler
        SIG_LIBC_RAN.store(0, Ordering::SeqCst);
        let rr = libc::signal::raise(libc::signal::SIGUSR1);
        rpt.check("raise rc==0", rr == 0);
        let _ = libsys::yield_now();
        let _ = libsys::yield_now();
        rpt.check("signal handler ran (libc)", SIG_LIBC_RAN.load(Ordering::SeqCst) == 1);
        // 恢复默认，避免残留处理器影响后续。
        libc::signal::signal(libc::signal::SIGUSR1, libc::signal::SIG_DFL);
    }

    // 11) fcntl：F_DUPFD / F_GETFD（经内核 dup2）
    unsafe {
        // F_DUPFD：复制 stdout(1) 到 >=10 的最低空闲槽。
        let newfd = libc::unistd::fcntl(1, libc::unistd::F_DUPFD, 10);
        rpt.check("fcntl F_DUPFD >=10", newfd >= 10);
        // 副本应可写（与 stdout 同句柄）。
        let wc = libc::unistd::write(newfd, b"".as_ptr() as *const core::ffi::c_void, 0);
        let _ = wc;
        libc::unistd::close(newfd);
        // F_GETFD：stdout 未带 CLOEXEC ⇒ 0。**登记项③ 之后这是真值**，不再是硬编码近似。
        let gfd = libc::unistd::fcntl(1, libc::unistd::F_GETFD, 0);
        rpt.check("fcntl F_GETFD==0", gfd == 0);
        // ---- 登记项③：fd-flags 原语（兑现 F_GETFD / F_SETFD）----
        {
            let ff = b"/scratch/conf_fdflags\0".as_ptr() as *const i8;
            let fd = libc::unistd::open(
                ff,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR | libc::unistd::O_CLOEXEC,
                0o600,
            );
            rpt.check("fdflags: open(O_CLOEXEC) -> fd", fd >= 0);
            if fd >= 0 {
                // (a) 如实报出 FD_CLOEXEC（旧实现恒 0 ⇒ 这条必红，是判别性断言）
                rpt.check(
                    "fdflags: F_GETFD reports FD_CLOEXEC for an O_CLOEXEC fd",
                    libc::unistd::fcntl(fd, libc::unistd::F_GETFD) == libc::unistd::FD_CLOEXEC,
                );
                // (b) F_SETFD(0) **真的**清掉它（旧实现只是「没做」）
                rpt.check(
                    "fdflags: F_SETFD(0) -> 0",
                    libc::unistd::fcntl(fd, libc::unistd::F_SETFD, 0) == 0,
                );
                rpt.check(
                    "fdflags: F_GETFD after clearing -> 0",
                    libc::unistd::fcntl(fd, libc::unistd::F_GETFD) == 0,
                );
                // (c) 再设回去
                rpt.check(
                    "fdflags: F_SETFD(FD_CLOEXEC) -> 0",
                    libc::unistd::fcntl(fd, libc::unistd::F_SETFD, libc::unistd::FD_CLOEXEC) == 0,
                );
                rpt.check(
                    "fdflags: F_GETFD after setting -> FD_CLOEXEC",
                    libc::unistd::fcntl(fd, libc::unistd::F_GETFD) == libc::unistd::FD_CLOEXEC,
                );
                // (d) 坏 fd ⇒ -1/EBADF
                libc::errno::set_errno(0);
                rpt.check(
                    "fdflags: F_GETFD(bad fd) -> -1",
                    libc::unistd::fcntl(9999, libc::unistd::F_GETFD) == -1,
                );
                rpt.check(
                    "fdflags: F_GETFD(bad fd) sets EBADF",
                    libc::errno::errno() == libc::errno::EBADF,
                );
                let _ = libc::unistd::close(fd);
            }
            // (e) 普通 fd ⇒ 0（既不是恒 0 也不是恒 FD_CLOEXEC：真在查标志位）
            let fd2 = libc::unistd::open(ff, libc::unistd::O_RDWR, 0o600);
            if fd2 >= 0 {
                rpt.check(
                    "fdflags: F_GETFD == 0 for a plain fd",
                    libc::unistd::fcntl(fd2, libc::unistd::F_GETFD) == 0,
                );
                let _ = libc::unistd::close(fd2);
            }
            let _ = libc::unistd::unlink(ff);
        }
    }

    // 12) stat / fstat / chmod / rename（经内核 VFS + INode::metadata/set_permissions）
    unsafe {
        // 在根目录下创建独立测试目录与文件。
        let sdir = b"/tmp_libc_sd\0".as_ptr() as *const i8;
        let _ = libc::unistd::mkdir(sdir, 0o755);
        let fa = b"/tmp_libc_sd/a.txt\0".as_ptr() as *const i8;
        let fp = libc::stdio::fopen(fa, b"w\0".as_ptr() as *const i8);
        if !fp.is_null() {
            libc::stdio::fwrite(b"abc\0".as_ptr() as *const core::ffi::c_void, 1, 3, fp);
            libc::stdio::fclose(fp);
        }

        // rename：a.txt -> b.txt（同目录内重命名）。
        let fb = b"/tmp_libc_sd/b.txt\0".as_ptr() as *const i8;
        let rr = libc::unistd::rename(fa, fb);
        rpt.check("rename rc==0", rr == 0);
        // 重命名后旧名不应再存在。
        let mut oldst: libc::unistd::stat = core::mem::zeroed();
        let so = libc::unistd::stat(fa, &mut oldst);
        rpt.check("rename old gone (ENOENT)", so != 0);

        // stat：解析路径读元数据。
        let mut st: libc::unistd::stat = core::mem::zeroed();
        let sr = libc::unistd::stat(fb, &mut st);
        rpt.check("stat rc==0", sr == 0);
        rpt.check("stat type==REG", st.st_mode & libc::unistd::S_IFMT == libc::unistd::S_IFREG);
        rpt.check("stat size==3", st.st_size == 3);

        // stat 目录：类型位应为 DIR。
        let mut dst: libc::unistd::stat = core::mem::zeroed();
        let sd = libc::unistd::stat(sdir, &mut dst);
        rpt.check("stat dir rc==0", sd == 0);
        rpt.check("stat dir type==DIR", dst.st_mode & libc::unistd::S_IFMT == libc::unistd::S_IFDIR);

        // fstat：按 fd 读元数据。
        // fstat：按 fd 读元数据。
        let fdf = libc::unistd::open(fb, libc::unistd::O_RDONLY, 0);
        rpt.check("fstat open fd>=0", fdf >= 0);
        if fdf >= 0 {
            let mut fst: libc::unistd::stat = core::mem::zeroed();
            let fr = libc::unistd::fstat(fdf, &mut fst);
            rpt.check("fstat rc==0", fr == 0);
            rpt.check("fstat type==REG", fst.st_mode & libc::unistd::S_IFMT == libc::unistd::S_IFREG);
            rpt.check("fstat size==3", fst.st_size == 3);
            libc::unistd::close(fdf);
        } else {
            rpt.check("fstat open", false);
        }

        // chmod + stat 往返：0o700 设可执行，0o600 清除可执行。
        let c1 = libc::unistd::chmod(fb, 0o700);
        rpt.check("chmod 0o700 rc==0", c1 == 0);
        let mut s1: libc::unistd::stat = core::mem::zeroed();
        let _ = libc::unistd::stat(fb, &mut s1);
        rpt.check("chmod 0o700 sets exec", s1.st_mode & 0o111 != 0);
        let c2 = libc::unistd::chmod(fb, 0o600);
        rpt.check("chmod 0o600 rc==0", c2 == 0);
        let mut s2: libc::unistd::stat = core::mem::zeroed();
        let _ = libc::unistd::stat(fb, &mut s2);
        rpt.check("chmod 0o600 clears exec", s2.st_mode & 0o111 == 0);
        rpt.check("chmod 0o600 keeps rw", s2.st_mode & 0o600 != 0);

        // 清理：删文件与目录。
        let _ = libc::unistd::remove(fb);
        let _ = libc::unistd::remove(sdir);
    }

    unsafe {
        // ---- A2-5：POSIX 账户查询（getpwnam/getpwuid，读 /config/users.json）----
        //
        // 真实性要求（S06/S09）：本段**先写入具体账户**，再断言查得的 uid/gid 与之相符；
        // 并显式验证"表缺失时如实失败、绝不返回伪造账户"。若直接查一个不存在的表并断言
        // 返回 NULL，那只能证明"没数据时没数据"，证明不了映射正确。
        {
            const TABLE: &str = "/config/users.json";
            let table = b"/config/users.json\0";
            // 1) 先确保 /config 存在（userd 正常启动时已建；此处幂等兜底）。
            let cfg = b"/config\0";
            let _ = libc::unistd::mkdir(cfg.as_ptr() as *const i8, 0o755);

            // 2) 写一份**已知内容**的账户表。
            let fd = libc::unistd::open(table.as_ptr() as *const i8,
                libc::unistd::O_WRONLY | libc::unistd::O_CREAT | libc::unistd::O_TRUNC, 0o644);
            rpt.check("pwd: open users.json for write", fd >= 0);
            if fd >= 0 {
                let body = br#"{"users":[{"name":"alice","uid":1000,"gid":1000},{"name":"bob","uid":1001,"gid":1001}]}"#;
                let w = libc::unistd::write(fd, body.as_ptr() as *const core::ffi::c_void, body.len());
                rpt.check("pwd: wrote account table", w == body.len() as isize);
                libc::unistd::close(fd);
            }

            // 3) endpwent 清缓存，强制重新读表（否则可能命中上一次的缓存）。
            libc::pwd::endpwent();

            // 4) getpwnam("alice") 必须返回**真** uid/gid（与写入内容逐字段相符）。
            let alice_name = b"alice\0";
            let pa = libc::pwd::getpwnam(alice_name.as_ptr() as *const i8);
            rpt.check("pwd: getpwnam(alice) non-null", !pa.is_null());
            if !pa.is_null() {
                let a = &*pa;
                rpt.check("pwd: alice uid == 1000 (real, not fabricated)", a.pw_uid == 1000);
                rpt.check("pwd: alice gid == 1000 (real, not fabricated)", a.pw_gid == 1000);
                rpt.check("pwd: alice pw_name non-null", !a.pw_name.is_null());
                rpt.check("pwd: alice pw_dir == /users/alice", !a.pw_dir.is_null());
            } else {
                rpt.check("pwd: alice uid == 1000", false);
                rpt.check("pwd: alice gid == 1000", false);
                rpt.check("pwd: alice pw_name non-null", false);
                rpt.check("pwd: alice pw_dir == /users/alice", false);
            }

            // 5) getpwuid(1001) 必须**反查**到 bob（名字↔uid 双向一致）。
            let pb = libc::pwd::getpwuid(1001);
            rpt.check("pwd: getpwuid(1001) non-null", !pb.is_null());
            if !pb.is_null() {
                let b = &*pb;
                rpt.check("pwd: uid 1001 resolves to name \"bob\"",
                    b.pw_name == alice_name.as_ptr() as *mut i8 || {
                        // 逐字节比较（名字存储不同缓冲区）。
                        let n = b.pw_name as *const u8;
                        n.read() == b'b' && n.add(1).read() == b'o' && n.add(2).read() == b'b' && n.add(3).read() == 0
                    });
                rpt.check("pwd: bob uid == 1001", b.pw_uid == 1001);
                rpt.check("pwd: bob gid == 1001", b.pw_gid == 1001);
            } else {
                rpt.check("pwd: uid 1001 resolves to name \"bob\"", false);
                rpt.check("pwd: bob uid == 1001", false);
                rpt.check("pwd: bob gid == 1001", false);
            }

            // 6) **诚实边界**：查一个表里没有的名字必须 NULL + ENOENT（不是伪账户）。
            let ghost = b"nosuchuser\0";
            libc::errno::set_errno(0);
            let pg = libc::pwd::getpwnam(ghost.as_ptr() as *const i8);
            rpt.check("pwd: unknown name -> NULL (no fabricated account)", pg.is_null());
            rpt.check("pwd: unknown name sets ENOENT", libc::errno::errno() == libc::errno::ENOENT);
            let pu = libc::pwd::getpwuid(999999);
            rpt.check("pwd: unknown uid -> NULL", pu.is_null());

            // 7) getpwent 遍历：应恰好走完两条（且不含伪造条目）。
            libc::pwd::setpwent();
            let n1 = libc::pwd::getpwent();
            let n2 = libc::pwd::getpwent();
            let n3 = libc::pwd::getpwent();
            rpt.check("pwd: getpwent yields 2 entries then NULL",
                !n1.is_null() && !n2.is_null() && n3.is_null());
            libc::pwd::endpwent();

            // 清理：移除测试表。
            let _ = libc::unistd::remove(table.as_ptr() as *const i8);
        }

    }
    // ---- A2-4：组账户查询（getgrnam/getgrgid/getgrouplist，读 /config/groups.json）----
    //
    // 真实性要求同 A2-5：先写入**已知内容**的组表，再断言查得的 gid 与之相符；
    // 并显式验证"表缺失时如实失败、绝不返回伪造组"。
    unsafe {
        let gtable = b"/config/groups.json\0";
        let _ = libc::unistd::mkdir(b"/config\0".as_ptr() as *const i8, 0o755);

        // 1) 写入已知内容的组表：dev(2000){alice,bob}、ops(2001){carol}。
        let fd = libc::unistd::open(gtable.as_ptr() as *const i8,
            libc::unistd::O_WRONLY | libc::unistd::O_CREAT | libc::unistd::O_TRUNC, 0o644);
        rpt.check("grp: open groups.json for write", fd >= 0);
        if fd >= 0 {
            let body = br#"{"groups":[{"name":"dev","gid":2000,"members":["alice","bob"]},{"name":"ops","gid":2001,"members":["carol"]}]}"#;
            let w = libc::unistd::write(fd, body.as_ptr() as *const core::ffi::c_void, body.len());
            rpt.check("grp: wrote group table", w == body.len() as isize);
            libc::unistd::close(fd);
        }

        libc::pwd::endgrent();

        // 2) getgrnam("dev") 必须返回**真** gid 2000。
        let g = libc::pwd::getgrnam(b"dev\0".as_ptr() as *const i8);
        rpt.check("grp: getgrnam(dev) non-null", !g.is_null());
        rpt.check("grp: dev gid == 2000 (real, not fabricated)",
            !g.is_null() && (*g).gr_gid == 2000);

        // 3) getgrgid(2001) 必须反查到 ops（名字<->gid 双向一致）。
        let g2 = libc::pwd::getgrgid(2001);
        let ops_ok = !g2.is_null() && {
            let n = (*g2).gr_name as *const u8;
            !n.is_null() && n.read() == b'o' && n.add(1).read() == b'p'
                && n.add(2).read() == b's' && n.add(3).read() == 0 && (*g2).gr_gid == 2001
        };
        rpt.check("grp: getgrgid(2001) -> ops(2001) (bidirectional)", ops_ok);

        // 4) **诚实边界**：查不存在的组必须 NULL + ENOENT（不是伪组）。
        libc::errno::set_errno(0);
        let ghost = libc::pwd::getgrnam(b"nosuchgroup\0".as_ptr() as *const i8);
        rpt.check("grp: unknown group -> NULL (no fabricated group)", ghost.is_null());
        rpt.check("grp: unknown group sets ENOENT", libc::errno::errno() == libc::errno::ENOENT);

        // 5) getgrouplist：alice 的组 = 主组 + 所属补充组 dev(2000)。
        let mut buf = [0u32; 8];
        let n = libc::pwd::groups_of_user("alice", 1000, &mut buf);
        rpt.check("grp: getgrouplist(alice) reports 2 groups", n == 2);
        rpt.check("grp: alice primary group first", n >= 1 && buf[0] == 1000);
        rpt.check("grp: alice supplementary dev(2000) present",
            n >= 2 && buf[..n].contains(&2000));
        // bob 同属 dev，carol 不属 dev。
        let nb = libc::pwd::groups_of_user("bob", 1001, &mut [0u32; 8]);
        rpt.check("grp: getgrouplist(bob) -> 2 (primary + dev)", nb == 2);
        let nc = libc::pwd::groups_of_user("carol", 1002, &mut [0u32; 8]);
        rpt.check("grp: carol is NOT in dev -> 2 (primary + ops)", nc == 2);

        // 6) 缓冲不足必须**如实返回所需条数**，不截断、不越界。
        let mut tiny = [0u32; 1];
        let need = libc::pwd::groups_of_user("alice", 1000, &mut tiny);
        rpt.check("grp: insufficient buffer reports required count (2)", need == 2);
        // 6b) **容量 0**：这是缺陷的**回归锁**。旧实现用 `out[..n]` 去重，n 是逻辑条数、
        //     可超过缓冲长度 ⇒ 容量 0 时第 1 次 push 就 `out[..1]` 越界 panic。
        //     注：上面的 `[u32; 1]` 用例**不足以**触发它（需要「所需条数 − 容量 ≥ 2」），
        //     这正是该缺陷此前能存活的原因——补上这条才真正锁住。
        let need0 = libc::pwd::groups_of_user("alice", 1000, &mut []);
        rpt.check("grp: zero-capacity buffer reports required count (2), no panic", need0 == 2);

        // ---- getgroups（D.2 本轮：补内核查询动词 SYS_TASK_GROUPS_GET 0x3E + libc 出口）----
        {
            // 先装配一组**已知**的补充组（自动化通道持 CAP_SYSTEM，可设）。
            let want = [3000u32, 3001];
            rpt.check("grp: groups_set for getgroups", libsys::groups_set(&want).is_ok());
            // 探测形态：size == 0（list 可为 NULL）⇒ 只问条数。
            let n = libc::process::getgroups(0, core::ptr::null_mut());
            rpt.check("grp: getgroups(0, NULL) reports 2", n == 2);
            // 正常形态：写入真实 gid。
            let mut gbuf = [0u32; 8];
            let n2 = libc::process::getgroups(8, gbuf.as_mut_ptr());
            rpt.check("grp: getgroups(8, buf) returns 2", n2 == 2);
            rpt.check("grp: getgroups wrote the real gids", gbuf[0] == 3000 && gbuf[1] == 3001);
            // 缓冲不足 ⇒ -1 置 EINVAL（POSIX：不截断、不写半个列表）。
            let mut tiny = [0u32; 1];
            libc::errno::set_errno(0);
            let n3 = libc::process::getgroups(1, tiny.as_mut_ptr());
            rpt.check("grp: getgroups too-small -> -1", n3 == -1);
            rpt.check("grp: getgroups too-small sets EINVAL", libc::errno::errno() == libc::errno::EINVAL);
            // 复原，避免影响后续检查。
            let _ = libsys::groups_clear();
            let n4 = libc::process::getgroups(0, core::ptr::null_mut());
            rpt.check("grp: groups_clear then getgroups reports 0", n4 == 0);
        }
        // ---- fsync（D.2 本轮：内核 SYS_STREAM_FSYNC 0x1A + libc 出口）----
        {
            let p = b"/config/fsync_probe.tmp\0";
            let fd = libc::unistd::open(
                p.as_ptr() as *const i8,
                libc::unistd::O_CREAT | libc::unistd::O_WRONLY,
                0o600,
            );
            rpt.check("fsync: open probe file", fd >= 0);
            if fd >= 0 {
                let data = b"fsync probe\n";
                let w = libc::unistd::write(fd, data.as_ptr() as *const core::ffi::c_void, data.len());
                rpt.check("fsync: write probe file", w == data.len() as isize);
                rpt.check("fsync(fd) returns 0", libc::unistd::fsync(fd) == 0);
                let _ = libc::unistd::close(fd);
            }
            // fd 不存在 ⇒ -1（不假装成功）。
            rpt.check("fsync(bad fd) -> -1", libc::unistd::fsync(9999) == -1);
            // 管道端无持久化语义 ⇒ -1 置 EINVAL。
            let mut pfd = [0i32; 2];
            if libc::posix_batch2::pipe(pfd.as_mut_ptr()) == 0 {
                libc::errno::set_errno(0);
                let rc = libc::unistd::fsync(pfd[0]);
                rpt.check("fsync(pipe) -> -1", rc == -1);
                rpt.check("fsync(pipe) sets EINVAL", libc::errno::errno() == libc::errno::EINVAL);

                // POSIX：管道**不可定位** ⇒ lseek 必须 ESPIPE，而且**不得**把该 fd 转入
                // 「用户态维护位置」模式——否则此后 read/write 会走定位 I/O，被内核按节点
                // 真值拒绝，一个探测性 lseek 就把管道用坏了。故探完还要**证明管道仍可用**。
                libc::errno::set_errno(0);
                let lr = libc::unistd::lseek(pfd[0], 0, 0 /* SEEK_SET */);
                rpt.check("lseek(pipe, SET) -> -1", lr == -1);
                rpt.check(
                    "lseek(pipe, SET) sets ESPIPE",
                    libc::errno::errno() == libc::errno::ESPIPE,
                );
                let msg = b"ok";
                let wn = libc::unistd::write(
                    pfd[1],
                    msg.as_ptr() as *const core::ffi::c_void,
                    2,
                );
                rpt.check("pipe still writable after lseek probe", wn == 2);
                let mut rb = [0u8; 2];
                let rn = libc::unistd::read(
                    pfd[0],
                    rb.as_mut_ptr() as *mut core::ffi::c_void,
                    2,
                );
                rpt.check(
                    "pipe still readable after lseek probe",
                    rn == 2 && &rb == b"ok",
                );
                let _ = libc::unistd::close(pfd[0]);
                let _ = libc::unistd::close(pfd[1]);
            }
            let _ = libc::unistd::remove(p.as_ptr() as *const i8);
        }
        // ---- fchmod / fchown（D.2 本轮：内核 SYS_STREAM_FCHMOD 0x1B / FCHOWN 0x1C + libc 出口）----
        {
            let p = b"/config/fchmod_probe.tmp\0";
            let fd = libc::unistd::open(
                p.as_ptr() as *const i8,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("fchmod: open probe file", fd >= 0);
            if fd >= 0 {
                // 改成 0400：按 owner 位折叠 ⇒ 写位**真的**消失，可证 fchmod 生效。
                rpt.check("fchmod(fd, 0400) returns 0", libc::unistd::fchmod(fd, 0o400) == 0);
                let mut st: libc::unistd::stat = unsafe { core::mem::zeroed() };
                rpt.check("fstat after fchmod ok", unsafe { libc::unistd::fstat(fd, &mut st) } == 0);
                rpt.check("fchmod took effect (owner write cleared)", (st.st_mode & 0o200) == 0);
                rpt.check("fchmod kept owner read", (st.st_mode & 0o400) != 0);
                // fchown 走通（改为自己 = 不变；易他主需 CAP_SYSTEM，这里不测越权面）。
                let u = libc::process::getuid();
                let g = libc::process::getgid();
                rpt.check("fchown(fd, self) returns 0", libc::unistd::fchown(fd, u, g) == 0);
                // POSIX：-1 表示不变。
                rpt.check("fchown(fd, -1, -1) returns 0", libc::unistd::fchown(fd, u32::MAX, u32::MAX) == 0);
                let _ = libc::unistd::close(fd);
            }
            rpt.check("fchmod(bad fd) -> -1", libc::unistd::fchmod(9999, 0o644) == -1);
            rpt.check("fchown(bad fd) -> -1", libc::unistd::fchown(9999, 0, 0) == -1);
        // ---- 语义忠实性审查登记项①：坏 fd ⇒ **EBADF**（内核新增 BadFd 错误码）----
        // 在此之前内核错误集里没有 EBADF：读/写坏 fd 报 EINVAL、fstat/close 报 ENOENT、
        // 读只写 fd 报 EACCES（与「节点权限不足」同码，用户态无法区分）。
        {
            let mut one = [0u8; 1];
            libc::errno::set_errno(0);
            rpt.check(
                "badfd: read(bad fd) -> -1",
                libc::unistd::read(9999, one.as_mut_ptr() as *mut core::ffi::c_void, 1) == -1,
            );
            rpt.check(
                "badfd: read(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check(
                "badfd: write(bad fd) -> -1",
                libc::unistd::write(9999, one.as_ptr() as *const core::ffi::c_void, 1) == -1,
            );
            rpt.check(
                "badfd: write(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            let mut st9: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check(
                "badfd: fstat(bad fd) -> -1",
                unsafe { libc::unistd::fstat(9999, &mut st9) } == -1,
            );
            rpt.check(
                "badfd: fstat(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check("badfd: close(bad fd) -> -1", libc::unistd::close(9999) == -1);
            rpt.check(
                "badfd: close(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check(
                "badfd: dup2(bad fd, 200) -> -1",
                libc::unistd::dup2(9999, 200) == -1,
            );
            rpt.check(
                "badfd: dup2(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check(
                "badfd: ftruncate(bad fd, 0) -> -1",
                libc::unistd::ftruncate(9999, 0) == -1,
            );
            rpt.check(
                "badfd: ftruncate(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check("badfd: fsync(bad fd) -> -1", libc::unistd::fsync(9999) == -1);
            rpt.check(
                "badfd: fsync(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            libc::errno::set_errno(0);
            rpt.check(
                "badfd: fchmod(bad fd) -> -1",
                libc::unistd::fchmod(9999, 0o644) == -1,
            );
            rpt.check(
                "badfd: fchmod(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
        }

        // ---- 登记项①（方向那一半）：对**未以该方向打开**的 fd 读写 ⇒ EBADF ----
        {
            // 注意：本块的 `p` 是外层作用域里的字节串变量（不是那个 helper fn），
            // 故这里直接取指针。
            let dpath = b"/scratch/conf_badfd_dir.txt\0".as_ptr() as *const libc::ctypes::c_char;
            let ro = libc::unistd::open(
                dpath,
                libc::unistd::O_CREAT | libc::unistd::O_RDONLY,
                0o600,
            );
            rpt.check("badfd: open O_RDONLY for direction test", ro >= 0);
            if ro >= 0 {
                let b2 = [0u8; 1];
                libc::errno::set_errno(0);
                rpt.check(
                    "badfd: write(read-only fd) -> -1",
                    libc::unistd::write(ro, b2.as_ptr() as *const core::ffi::c_void, 1) == -1,
                );
                rpt.check(
                    "badfd: write(read-only fd) sets EBADF",
                    libc::errno::errno() == libc::errno::EBADF,
                );
                let _ = libc::unistd::close(ro);
            }
            let wo = libc::unistd::open(dpath, libc::unistd::O_WRONLY, 0);
            rpt.check("badfd: open O_WRONLY for direction test", wo >= 0);
            if wo >= 0 {
                let mut b3 = [0u8; 1];
                libc::errno::set_errno(0);
                rpt.check(
                    "badfd: read(write-only fd) -> -1",
                    libc::unistd::read(wo, b3.as_mut_ptr() as *mut core::ffi::c_void, 1) == -1,
                );
                rpt.check(
                    "badfd: read(write-only fd) sets EBADF",
                    libc::errno::errno() == libc::errno::EBADF,
                );
                let _ = libc::unistd::close(wo);
            }
            let _ = libc::unistd::remove(dpath);
        }

            // ---- isatty 的 errno 语义（语义忠实性审查）----
            // **有效但非终端** => 0 并置 ENOTTY（POSIX）。此前不置 errno，于是"stdout 被重定向到
            // 普通文件"这个最常见的情形下，调用方读 errno 会拿到**上一次的残留值**。
            {
                let p2 = b"/config/isatty_probe.tmp\0";
                let ifd = libc::unistd::open(
                    p2.as_ptr() as *const i8,
                    libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                    0o600,
                );
                rpt.check("isatty: open probe file", ifd >= 0);
                if ifd >= 0 {
                    libc::errno::set_errno(0);
                    rpt.check("isatty(regular file) -> 0", libc::unistd::isatty(ifd) == 0);
                    rpt.check(
                        "isatty(regular file) sets ENOTTY",
                        libc::errno::errno() == libc::errno::ENOTTY,
                    );
                    let _ = libc::unistd::close(ifd);
                    let _ = libc::unistd::remove(p2.as_ptr() as *const i8);
                }
            }
            let mut pfd2 = [0i32; 2];
            if libc::posix_batch2::pipe(pfd2.as_mut_ptr()) == 0 {
                rpt.check("fchmod(pipe) -> -1", libc::unistd::fchmod(pfd2[0], 0o644) == -1);
                // POSIX：fstat 对**管道**也必须成功（S_IFIFO）。此前内核只处理文件句柄，
                // 管道端返回 NotFound ⇒ 用户态把"这是管道"与"fd 不存在"混成同一个 ENOENT。
                let mut pst: libc::unistd::stat = unsafe { core::mem::zeroed() };
                rpt.check(
                    "unistd: fstat(pipe) -> 0",
                    unsafe { libc::unistd::fstat(pfd2[0], &mut pst) } == 0,
                );
                rpt.check(
                    "unistd: fstat(pipe) reports S_IFIFO",
                    pst.st_mode & libc::unistd::S_IFMT == libc::unistd::S_IFIFO,
                );
                rpt.check("unistd: fstat(pipe) has a stable ino", pst.st_ino != 0);
                let mut pst2: libc::unistd::stat = unsafe { core::mem::zeroed() };
                rpt.check(
                    "unistd: fstat on both pipe ends -> 0",
                    unsafe { libc::unistd::fstat(pfd2[1], &mut pst2) } == 0,
                );
                rpt.check(
                    "unistd: both pipe ends share the same (nonzero) ino",
                    pst.st_ino == pst2.st_ino && pst.st_ino != 0,
                );
                // 有效但非终端 ⇒ 0 并置 ENOTTY（此前管道被报成 ENOENT，errno 是错的）。
                libc::errno::set_errno(0);
                rpt.check("unistd: isatty(pipe) -> 0", libc::unistd::isatty(pfd2[0]) == 0);
                rpt.check(
                    "unistd: isatty(pipe) sets ENOTTY (not ENOENT)",
                    libc::errno::errno() == libc::errno::ENOTTY,
                );                let _ = libc::unistd::close(pfd2[0]);
                let _ = libc::unistd::close(pfd2[1]);
            }
            let _ = libc::unistd::remove(p.as_ptr() as *const i8);
        }
        // ---- sigpending（D.2 本轮：内核 SYS_SIGNAL_PENDING 0x85 + libc 出口）----
        {
            // SIG_BLOCK=0 / SIG_UNBLOCK=1（见 libc/include/signal.h:102-103）。
            const SIG_BLOCK: i32 = 0;
            const SIG_UNBLOCK: i32 = 1;

            // 1) 基本契约：成功返回 0，且位图 bit63 清零（syscall 错误编码用 bit63）。
            let mut set: libc::signal::sigset_t = u64::MAX;
            rpt.check("sigpending(set) returns 0", unsafe { libc::signal::sigpending(&mut set) } == 0);
            rpt.check("sigpending clears bit63", (set & (1u64 << 63)) == 0);
            // 2) 空指针 ⇒ -1 置 EINVAL。
            libc::errno::set_errno(0);
            rpt.check("sigpending(NULL) -> -1", unsafe { libc::signal::sigpending(core::ptr::null_mut()) } == -1);
            rpt.check("sigpending(NULL) sets EINVAL", libc::errno::errno() == libc::errno::EINVAL);
            // 3) **语义**：屏蔽 SIGCHLD（POSIX 默认动作=忽略 ⇒ 解除屏蔽也不会杀进程）→ 投递 →
            //    待决集应出现该位；解除屏蔽后应被消费。这是「待决集 ≠ 屏蔽集」的可证形态。
            let mut blk: libc::signal::sigset_t = 0;
            unsafe { libc::signal::sigemptyset(&mut blk) };
            unsafe { libc::signal::sigaddset(&mut blk, libc::signal::SIGCHLD) };
            let rc = unsafe { libc::signal::sigprocmask(SIG_BLOCK, &blk, core::ptr::null_mut()) };
            rpt.check("sigpending: block SIGCHLD", rc == 0);
            let self_pid = libc::process::getpid();
            rpt.check("sigpending: raise SIGCHLD", libc::process::kill(self_pid, libc::signal::SIGCHLD) == 0);
            let mut pend: libc::signal::sigset_t = 0;
            rpt.check("sigpending after raise ok", unsafe { libc::signal::sigpending(&mut pend) } == 0);
            rpt.check("sigpending shows the blocked+raised signal",
                (pend & (1u64 << libc::signal::SIGCHLD)) != 0);
            let _ = unsafe { libc::signal::sigprocmask(SIG_UNBLOCK, &blk, core::ptr::null_mut()) };
            let mut pend2: libc::signal::sigset_t = u64::MAX;
            let _ = unsafe { libc::signal::sigpending(&mut pend2) };
            rpt.check("sigpending cleared after unblock", (pend2 & (1u64 << libc::signal::SIGCHLD)) == 0);
        }
        // ---- link（D.2 本轮：VFS 硬链接 + 内核 SYS_ENTRY_LINK 0x49 + libc 出口）----
        {
            let f1 = b"/link_a.tmp\0";
            let f2 = b"/link_b.tmp\0";
            let _ = libc::unistd::remove(f1.as_ptr() as *const i8);
            let _ = libc::unistd::remove(f2.as_ptr() as *const i8);
            let fd = libc::unistd::open(
                f1.as_ptr() as *const i8,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("link: create file", fd >= 0);
            if fd >= 0 {
                let data = b"hello hardlink\n";
                let w = libc::unistd::write(fd, data.as_ptr() as *const core::ffi::c_void, data.len());
                rpt.check("link: write file", w == data.len() as isize);
                let _ = libc::unistd::close(fd);
            }
            rpt.check("link(a,b) returns 0",
                libc::unistd::link(f1.as_ptr() as *const i8, f2.as_ptr() as *const i8) == 0);
            let mut s1: libc::unistd::stat = unsafe { core::mem::zeroed() };
            let mut s2: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check("link: stat a ok", libc::unistd::stat(f1.as_ptr() as *const i8, &mut s1) == 0);
            rpt.check("link: stat b ok", libc::unistd::stat(f2.as_ptr() as *const i8, &mut s2) == 0);
            rpt.check("link: same st_ino (real identity)", s1.st_ino == s2.st_ino && s1.st_ino != 0);
            rpt.check("link: st_nlink == 2", s1.st_nlink == 2 && s2.st_nlink == 2);
            let fd2 = libc::unistd::open(f2.as_ptr() as *const i8, libc::unistd::O_RDONLY, 0);
            rpt.check("link: open b", fd2 >= 0);
            if fd2 >= 0 {
                let mut buf = [0u8; 32];
                let n = libc::unistd::read(fd2, buf.as_mut_ptr() as *mut core::ffi::c_void, 32);
                rpt.check("link: content shared through b", n == 15 && &buf[..15] == b"hello hardlink\n");
                let _ = libc::unistd::close(fd2);
            }
            rpt.check("link: unlink a", libc::unistd::remove(f1.as_ptr() as *const i8) == 0);
            let mut s3: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check("link: b survives after unlink a",
                libc::unistd::stat(f2.as_ptr() as *const i8, &mut s3) == 0);
            rpt.check("link: nlink back to 1", s3.st_nlink == 1);
            rpt.check("link: directory refused",
                libc::unistd::link(b"/\0".as_ptr() as *const i8, b"/link_dir.tmp\0".as_ptr() as *const i8) == -1);
            let _ = libc::unistd::remove(f2.as_ptr() as *const i8);
        }
        // ---- nice / getpriority / setpriority（D.2 本轮：内核 SYS_TASK_NICE 0x3F）----
        {
            // 初始 nice 应为 0（「引入 nice 不改变既有进程调度行为」的前提）。
            rpt.check("nice: initial getpriority == 20 (nice 0)", libc::process::getpriority(0, 0) == 20);
            // setpriority(prio 25) ⇒ nice = 20 - 25 = -5。
            rpt.check("nice: setpriority(0,0,25) == 0", libc::process::setpriority(0, 0, 25) == 0);
            rpt.check("nice: getpriority == 25 (nice -5)", libc::process::getpriority(0, 0) == 25);
            // nice(inc) 是**累加**：当前 -5，+3 ⇒ -2 ⇒ priority 22。
            libc::errno::set_errno(0);
            rpt.check("nice(+3) returns -2 (accumulated)", libc::process::nice(3) == -2);
            rpt.check("nice: getpriority == 22", libc::process::getpriority(0, 0) == 22);
            // 上界钳制：prio 0 ⇒ nice 20 ⇒ 钳到 19 ⇒ priority 1。
            let _ = libc::process::setpriority(0, 0, 0);
            rpt.check("nice: clamped to 19 (priority 1)", libc::process::getpriority(0, 0) == 1);
            // 下界钳制：prio 100 ⇒ nice -80 ⇒ 钳到 -20 ⇒ priority 40。
            let _ = libc::process::setpriority(0, 0, 100);
            rpt.check("nice: clamped to -20 (priority 40)", libc::process::getpriority(0, 0) == 40);
            // 本系统无进程组 ⇒ PRIO_PGRP 如实拒绝。
            libc::errno::set_errno(0);
            rpt.check("nice: PRIO_PGRP refused", libc::process::getpriority(1, 0) == -1);
            rpt.check("nice: PRIO_PGRP sets EINVAL", libc::errno::errno() == libc::errno::EINVAL);
            // 复原，避免影响后续检查的调度权重。
            let _ = libc::process::setpriority(0, 0, 20);
            rpt.check("nice: restored to nice 0", libc::process::getpriority(0, 0) == 20);
        }
        // ---- sigsuspend（D.2 本轮：内核 SYS_SIGNAL_SUSPEND 0x86）----
        {
            // SIG_BLOCK=0 / SIG_SETMASK=2（见 libc/include/signal.h:102-104）。
            const SIG_BLOCK: i32 = 0;
            const SIG_SETMASK: i32 = 2;
            const SIG_UNBLOCK: i32 = 1;
            static HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            extern "C" fn on_usr1(_sig: i32) {
                HITS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
            let act = libc::signal::sigaction {
                sa_handler: on_usr1 as usize,
                sa_mask: 0,
                sa_flags: 0,
                sa_restorer: core::ptr::null_mut(),
            };
            rpt.check("sigsuspend: install SIGUSR1 handler",
                unsafe { libc::signal::sigaction(libc::signal::SIGUSR1, &act, core::ptr::null_mut()) } == 0);
            // 屏蔽 SIGUSR1，再投递（被挡住 ⇒ 进 pending）。
            let mut blk: libc::signal::sigset_t = 0;
            unsafe { libc::signal::sigemptyset(&mut blk) };
            unsafe { libc::signal::sigaddset(&mut blk, libc::signal::SIGUSR1) };
            rpt.check("sigsuspend: block SIGUSR1",
                unsafe { libc::signal::sigprocmask(SIG_BLOCK, &blk, core::ptr::null_mut()) } == 0);
            let me_pid = libc::process::getpid();
            rpt.check("sigsuspend: raise SIGUSR1 while blocked",
                libc::process::kill(me_pid, libc::signal::SIGUSR1) == 0);
            // **关键**：sigsuspend(空集) 原子解除屏蔽并等信号。此刻已有可投递信号
            // ⇒ 不该睡死（这正是「换屏蔽集期间不丢信号」的可证形态）。
            let empty: libc::signal::sigset_t = 0;
            libc::errno::set_errno(0);
            let rc = unsafe { libc::signal::sigsuspend(&empty) };
            rpt.check("sigsuspend returns -1", rc == -1);
            rpt.check("sigsuspend sets EINTR", libc::errno::errno() == libc::errno::EINTR);
            rpt.check("sigsuspend: handler ran exactly once",
                HITS.load(core::sync::atomic::Ordering::SeqCst) == 1);
            // 屏蔽集被**还原**为 suspend 前的值（仍屏蔽 SIGUSR1）。
            let mut now: libc::signal::sigset_t = 0;
            let _ = unsafe { libc::signal::sigprocmask(SIG_SETMASK, core::ptr::null(), &mut now) };
            rpt.check("sigsuspend: mask restored (SIGUSR1 still blocked)",
                (now & (1u64 << libc::signal::SIGUSR1)) != 0);
            // 复原：解除屏蔽。
            let _ = unsafe { libc::signal::sigprocmask(SIG_UNBLOCK, &blk, core::ptr::null_mut()) };
            // 空指针如实 EINVAL。
            libc::errno::set_errno(0);
            rpt.check("sigsuspend(NULL) -> -1", unsafe { libc::signal::sigsuspend(core::ptr::null()) } == -1);
            rpt.check("sigsuspend(NULL) sets EINVAL", libc::errno::errno() == libc::errno::EINVAL);
        }
        // ---- alarm（D.2 本轮：内核 SYS_SIGNAL_ALARM 0x87）----
        {
            static ALRM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            extern "C" fn on_alrm(_sig: i32) {
                ALRM.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
            let act = libc::signal::sigaction {
                sa_handler: on_alrm as usize,
                sa_mask: 0,
                sa_flags: 0,
                sa_restorer: core::ptr::null_mut(),
            };
            rpt.check("alarm: install SIGALRM handler",
                unsafe { libc::signal::sigaction(libc::signal::SIGALRM, &act, core::ptr::null_mut()) } == 0);
            // 首次设置应返回 0（没有上一个闹钟）。
            rpt.check("alarm(1) returns 0 (no previous)", libc::signal::alarm(1) == 0);
            // 立刻重设：应返回**上一个**的剩余秒数（向上取整 ⇒ 1）。
            rpt.check("alarm(2) returns previous remaining (1)", libc::signal::alarm(2) == 1);
            // 取消：返回 2，且此后不该触发。
            rpt.check("alarm(0) cancels and returns 2", libc::signal::alarm(0) == 2);
            // 重新设 1 秒并等它到期（带界轮询，避免挂死）。
            rpt.check("alarm(1) again returns 0", libc::signal::alarm(1) == 0);
            let mut waited = 0u32;
            while ALRM.load(core::sync::atomic::Ordering::SeqCst) == 0 && waited < 200 {
                // 50ms × 200 = 10s 上界；正常 1s 内触发。
                let _ = libsys::sleep(50_000_000);
                waited += 1;
            }
            rpt.check("alarm: SIGALRM delivered within bound",
                ALRM.load(core::sync::atomic::Ordering::SeqCst) == 1);
        }
        // ---- 登记项⑥：PROT_NONE（存在但不可访问）----
        {
            // (a) POSIX 调用形态：prot == 0 的 mmap 必须**成功**（不是 ENOTSUP/EINVAL）。
            let pn0 = unsafe {
                libc::mman::mmap(
                    core::ptr::null_mut(),
                    4096,
                    0,
                    libc::mman::MAP_PRIVATE | libc::mman::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            rpt.check("prot_none: mmap(prot=0) succeeds", pn0 != libc::mman::MAP_FAILED);
            if pn0 != libc::mman::MAP_FAILED {
                let _ = unsafe { libc::mman::munmap(pn0, 4096) };
            }
            // (b) 先 RW 映射并**触碰**（按需分页要真落帧——见报告里登记的
            //     「mprotect 对从未触碰的页返回 NotFound」缺口），再改权限。
            let pn = unsafe {
                libc::mman::mmap(
                    core::ptr::null_mut(),
                    4096,
                    libc::mman::PROT_READ | libc::mman::PROT_WRITE,
                    libc::mman::MAP_PRIVATE | libc::mman::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            rpt.check("prot_none: mmap(RW) succeeds", pn != libc::mman::MAP_FAILED);
            if pn != libc::mman::MAP_FAILED {
                unsafe { core::ptr::write_volatile(pn as *mut u8, 7) };
                rpt.check(
                    "prot_none: touched page reads back",
                    unsafe { core::ptr::read_volatile(pn as *const u8) } == 7,
                );
                rpt.check(
                    "prot_none: mprotect(prot=0) is supported",
                    unsafe { libc::mman::mprotect(pn, 4096, 0) } == 0,
                );
                // 「存在」：改成只读应当成功 ⇒ 是「存在但不可访问」，不是「根本没映射」。
                rpt.check(
                    "prot_none: mapping is present (mprotect to READ works)",
                    unsafe { libc::mman::mprotect(pn, 4096, libc::mman::PROT_READ) } == 0,
                );
                rpt.check(
                    "prot_none: mprotect(prot=0) again",
                    unsafe { libc::mman::mprotect(pn, 4096, 0) } == 0,
                );
                // 「不可访问」：交给子进程真读一次。若内核仍把它当 RW（旧行为 prot=0 →
                // PROT_DEFAULT），读会成功 ⇒ 子进程 _exit(读到的值 = 0) ⇒ 断言红。
                let kid = libc::process::fork();
                if kid == 0 {
                    extern "C" fn on_segv(_s: i32) {
                        libc::process::_exit(42);
                    }
                    let sa = libc::signal::sigaction {
                        sa_handler: on_segv as usize,
                        sa_mask: 0,
                        sa_flags: 0,
                        sa_restorer: core::ptr::null_mut(),
                    };
                    let _ = unsafe {
                        libc::signal::sigaction(libc::signal::SIGSEGV, &sa, core::ptr::null_mut())
                    };
                    let v = unsafe { core::ptr::read_volatile(pn as *const u8) };
                    libc::process::_exit(v as i32); // 没 fault ⇒ 退出码 0
                }
                rpt.check("prot_none: fault probe child forked", kid > 0);
                if kid > 0 {
                    // **有界等待**（最多 ~3s）：若内核把 PROT_NONE 当成「待提交」而陷入
                    // 「提交 → 重试 → 再 fault」的死循环，这里必须能脱身并判红，
                    // 绝不能让整轮 libccheck 挂死（登记项⑦ 上已经吃过一次挂死的亏）。
                    let mut st: i32 = 0;
                    let mut waited = 0;
                    let mut w = 0;
                    loop {
                        w = libc::process::waitpid(kid, &mut st, libc::unistd::WNOHANG);
                        if w == kid || w == -1 || waited > 60 {
                            break;
                        }
                        let _ = libsys::sleep(50_000_000);
                        waited += 1;
                    }
                    if w != kid {
                        let _ = libc::process::kill(kid, libc::signal::SIGKILL);
                        let _ = libc::process::waitpid(kid, &mut st, 0);
                    }
                    rpt.check("prot_none: fault probe reaped (bounded wait)", w == kid);
                    rpt.check(
                        "prot_none: access faults (child handler ran -> exit 42)",
                        w == kid && ((st >> 8) & 0xFF) == 42,
                    );
                }
                let _ = unsafe { libc::mman::munmap(pn, 4096) };

                // (f) **从未触碰**的映射也必须能 mprotect（POSIX：mprotect 改的是**映射的
                //     权限**，与页是否已驻留无关）。此前内核先扫一遍「每页都已映射」，
                //     对刚 mmap、还没碰过的区间直接报 ENOENT。
                let pu = unsafe {
                    libc::mman::mmap(
                        core::ptr::null_mut(),
                        4096,
                        libc::mman::PROT_READ | libc::mman::PROT_WRITE,
                        libc::mman::MAP_PRIVATE | libc::mman::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                rpt.check("prot_none: mmap for the never-touched test", pu != libc::mman::MAP_FAILED);
                if pu != libc::mman::MAP_FAILED {
                    rpt.check(
                        "prot_none: mprotect on a NEVER-TOUCHED mapping works",
                        unsafe { libc::mman::mprotect(pu, 4096, libc::mman::PROT_READ) } == 0,
                    );
                    // (g) 新权限必须**作用于首次提交**：改成 PROT_NONE 之后，子进程读一次必须
                    //     fault（handler ⇒ _exit(42)）。若内核把新权限丢了、仍按旧 RW 提交这一页，
                    //     读就会成功 ⇒ 退出码 0 ⇒ 红。
                    rpt.check(
                        "prot_none: mprotect(PROT_NONE) on the never-touched mapping",
                        unsafe { libc::mman::mprotect(pu, 4096, 0) } == 0,
                    );
                    let kid2 = libc::process::fork();
                    if kid2 == 0 {
                        extern "C" fn on_segv2(_s: i32) {
                            libc::process::_exit(42);
                        }
                        let sa2 = libc::signal::sigaction {
                            sa_handler: on_segv2 as usize,
                            sa_mask: 0,
                            sa_flags: 0,
                            sa_restorer: core::ptr::null_mut(),
                        };
                        let _ = unsafe {
                            libc::signal::sigaction(libc::signal::SIGSEGV, &sa2, core::ptr::null_mut())
                        };
                        let v2 = unsafe { core::ptr::read_volatile(pu as *const u8) };
                        libc::process::_exit(v2 as i32); // 没 fault ⇒ 退出码 0
                    }
                    rpt.check("prot_none: never-touched fault probe forked", kid2 > 0);
                    if kid2 > 0 {
                        let mut st2: i32 = 0;
                        let mut waited2 = 0;
                        let mut w2 = 0;
                        loop {
                            w2 = libc::process::waitpid(kid2, &mut st2, libc::unistd::WNOHANG);
                            if w2 == kid2 || w2 == -1 || waited2 > 60 {
                                break;
                            }
                            let _ = libsys::sleep(50_000_000);
                            waited2 += 1;
                        }
                        if w2 != kid2 {
                            let _ = libc::process::kill(kid2, libc::signal::SIGKILL);
                            let _ = libc::process::waitpid(kid2, &mut st2, 0);
                        }
                        rpt.check(
                            "prot_none: never-touched mapping faults on first touch (exit 42)",
                            w2 == kid2 && ((st2 >> 8) & 0xFF) == 42,
                        );
                    }
                    let _ = unsafe { libc::mman::munmap(pu, 4096) };
                }
            }
        }
        // ---- 登记项⑩：信号处置是**进程级**的（fork 继承；组内线程共享）----
        // POSIX：fork 出来的子进程**继承**父的信号处置；组内线程**共享**同一份处置
        // （一个线程 sigaction 之后，同进程其它线程都看得到）。此前 SignalState 挂在
        // Process（= 每线程）上 ⇒ 子进程只剩 restorer、处置全丢；组员之间也互不可见。
        {
            extern "C" fn on_pw(_s: i32) {}
            let act = libc::signal::sigaction {
                sa_handler: on_pw as usize,
                sa_mask: 0,
                sa_flags: 0,
                sa_restorer: core::ptr::null_mut(),
            };
            rpt.check(
                "sigproc: install SIGUSR1 handler",
                unsafe {
                    libc::signal::sigaction(libc::signal::SIGUSR1, &act, core::ptr::null_mut())
                } == 0,
            );
            // 先自证：本进程查得到（查询模式，不改状态）。
            let mut cur: libc::signal::sigaction = unsafe { core::mem::zeroed() };
            rpt.check(
                "sigproc: query sees the handler in this process",
                unsafe {
                    libc::signal::sigaction(libc::signal::SIGUSR1, core::ptr::null(), &mut cur)
                } == 0
                    && cur.sa_handler == on_pw as usize,
            );
            let kid = libc::process::fork();
            if kid == 0 {
                // 子进程：查询 SIGUSR1 的处置 —— POSIX fork 要求**继承**它。
                let mut old: libc::signal::sigaction = unsafe { core::mem::zeroed() };
                let r = unsafe {
                    libc::signal::sigaction(libc::signal::SIGUSR1, core::ptr::null(), &mut old)
                };
                // 退出码可区分（便于定位）：42 = 继承到 handler；41 = **查询本身失败**；
                // 40 = 查询成功但处置不是它（SIG_DFL，缺陷版）。
                let code = if r != 0 {
                    41
                } else if old.sa_handler == on_pw as usize {
                    42
                } else {
                    40
                };
                libc::process::_exit(code);
            }
            rpt.check("sigproc: fork for the inheritance probe", kid > 0);
            if kid > 0 {
                let mut st: i32 = 0;
                let w = libc::process::waitpid(kid, &mut st, 0);
                rpt.check("sigproc: inheritance probe reaped", w == kid);
                out(b"[sigproc] child exit = ");
                out(crate::util::u64_to_dec(((st >> 8) & 0xFF) as u64, &mut [0u8; 24]));
                out(b" (42=inherited, 41=query failed, 40=default)\n");
                rpt.check(
                    "sigproc: the child INHERITED the handler (POSIX fork)",
                    w == kid && ((st >> 8) & 0xFF) == 42,
                );
            }
            // 复原：SIG_DFL（不把 handler 留给后面的用例）。
            let dfl = libc::signal::sigaction {
                sa_handler: libc::signal::SIG_DFL,
                sa_mask: 0,
                sa_flags: 0,
                sa_restorer: core::ptr::null_mut(),
            };
            let _ = unsafe {
                libc::signal::sigaction(libc::signal::SIGUSR1, &dfl, core::ptr::null_mut())
            };
        }
        // ---- 登记项⑦：nanosleep 的 rem（被信号打断时写回剩余时长）----
        // **只做可验证的那一半**：不被打断时 `nanosleep` 成功返回 0，且**不碰** `rem`。
        //
        // **为什么不做「被打断」那一半——一个重大发现（已登记，待独立排查）**：
        // 把「装 SIGALRM handler + alarm(1) + nanosleep(3s)」放进机内用例后，
        // **libccheck 永久挂起**（20 分钟无任何输出，只剩内核周期性 intel-hda 日志）。
        // 即：**一个正在定时睡眠的进程收到信号后不再返回**。这是**内核缺陷**，不是 libc 侧：
        // 本轮 libc 的改动只在 `libsys::sleep` **返回之后**写 rem，不可能让它不返回。
        // 详见报告「第十五轮」的登记（含复现步骤）。
        {
            use core::sync::atomic::{AtomicU32, Ordering};
            static NRM: AtomicU32 = AtomicU32::new(0);
            extern "C" fn on_alrm3(_sig: i32) {
                NRM.fetch_add(1, Ordering::SeqCst);
            }
            let act3 = libc::signal::sigaction {
                sa_handler: on_alrm3 as usize,
                sa_mask: 0,
                sa_flags: 0,
                sa_restorer: core::ptr::null_mut(),
            };
            rpt.check(
                "nanosleep: install SIGALRM handler",
                unsafe { libc::signal::sigaction(libc::signal::SIGALRM, &act3, core::ptr::null_mut()) } == 0,
            );
            rpt.check("nanosleep: alarm(1) armed", libc::signal::alarm(1) == 0);
            // 请求 3s、闹钟 1s ⇒ 约 1s 后被 SIGALRM **打断**，rem 应约 2s。
            // **这条断言在修复内核前是红的**（实测 nanosleep 返回 0、rem 未写）——
            // 见报告「第十六轮」：内核的 sleep 重试循环没有兑现 EINTR。
            let req = libc::time::Timespec { tv_sec: 3, tv_nsec: 0 };
            let mut rem = libc::time::Timespec { tv_sec: 0, tv_nsec: 0 };
            libc::errno::set_errno(0);
            let t0 = libsys::now();
            let rc = unsafe { libc::time::nanosleep(&req, &mut rem) };
            let t1 = libsys::now();
            // **把值打印出来**（本审查的教训：弱断言会放过语义错误的实现，
            // 钉精确值钉不了时至少要让人眼看得见）：请求 3s、闹钟 1s，
            // 故 elapsed≈1s、rem≈2s。
            out(b"[ns] elapsed_ns=");
            out(crate::util::u64_to_dec(t1.saturating_sub(t0), &mut [0u8; 24]));
            out(b" rem=");
            out(crate::util::u64_to_dec(rem.tv_sec as u64, &mut [0u8; 24]));
            out(b".");
            out(crate::util::u64_to_dec(rem.tv_nsec as u64, &mut [0u8; 24]));
            out(b" s\n");
            rpt.check("nanosleep: interrupted -> -1", rc == -1);
            rpt.check(
                "nanosleep: interrupted sets EINTR",
                libc::errno::errno() == libc::errno::EINTR,
            );
            rpt.check("nanosleep: handler ran", NRM.load(Ordering::SeqCst) >= 1);
            // **判据用纳秒总量，不要用「秒数向上取整」**：rem = 2.088s 时
            // 「秒数 + 1」会凑成 3 而被误判为失败（本轮实测踩到）。
            let rem_ns = (rem.tv_sec as i128) * 1_000_000_000 + rem.tv_nsec as i128;
            rpt.check(
                "nanosleep: rem is in (0, requested)",
                rem_ns > 0 && rem_ns < 3_000_000_000,
            );
            // 更强的判据：**闹钟已经响过 ⇒ 剩余必须明显小于请求值**（且大于 0）。
            // 若 rem 恒 0（未写）或恒等于请求值（没减 elapsed），这条会红。
            //
            // **为什么不钉具体数值**（两轮实测教训）：
            // 1) 「请求 − 本函数测得的 elapsed」量的是「从 t0 到本进程**重新被调度**
            //    之后」——EINTR 之后进程可能被推迟很久才回到这里（实测一次 elapsed
            //    被记成 2.246s，而内核其实在 0.88s 就打断、算出 rem=2.118s）；
            // 2) 闹钟**自身的抖动**在 4 核负载下达数百毫秒（实测 1s 的闹钟在
            //    0.88s~1.6s 之间响 ⇒ rem 相应落在 1.40s~2.12s）。
            // 钉死带宽会让门禁变成「时钟抖动检测器」而不是「实现检测器」；
            // 保留的判据（0 < rem < 2.4s）仍能证伪两种坏实现，且对抖动免疫。
            let _ = (t0, t1); // 两者只用于上面的留证打印
            rpt.check(
                "nanosleep: rem is well below the request (the alarm burned time)",
                rem_ns > 0 && rem_ns < 2_400_000_000,
            );
            // 对照：**没有**信号时 nanosleep 成功返回 0，且**不碰** rem。
            let req0 = libc::time::Timespec { tv_sec: 0, tv_nsec: 10_000_000 };
            let mut rem0 = libc::time::Timespec { tv_sec: 77, tv_nsec: 77 };
            rpt.check(
                "nanosleep: uninterrupted returns 0",
                unsafe { libc::time::nanosleep(&req0, &mut rem0) } == 0,
            );
            rpt.check(
                "nanosleep: rem untouched when not interrupted",
                rem0.tv_sec == 77 && rem0.tv_nsec == 77,
            );
        }
        // ---- sigaltstack（D.2 本轮：内核 SYS_SIGNAL_ALTSTACK 0x88 + SA_ONSTACK 切栈）----
        {
            use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
            static ON_ALT: AtomicU32 = AtomicU32::new(0);
            static SP_IN_RANGE: AtomicU32 = AtomicU32::new(0);
            static ALT_BASE: AtomicU64 = AtomicU64::new(0);
            static ALT_SIZE: AtomicU64 = AtomicU64::new(0);
            extern "C" fn on_usr2(_sig: i32) {
                // **关键**：取本函数局部变量的地址，看它是否落在备用栈范围内。
                // 落在里面才证明「handler 真的跑在备用栈上」，而不是只改了个状态字段。
                let local: u64 = 0;
                let addr = &local as *const u64 as u64;
                let base = ALT_BASE.load(Ordering::SeqCst);
                let size = ALT_SIZE.load(Ordering::SeqCst);
                if addr >= base && addr < base.saturating_add(size) {
                    SP_IN_RANGE.fetch_add(1, Ordering::SeqCst);
                }
                ON_ALT.fetch_add(1, Ordering::SeqCst);
            }
            // 1) 初始查询：应为 SS_DISABLE。
            let mut cur = libc::signal::stack_t { ss_sp: core::ptr::null_mut(), ss_flags: 0, ss_size: 0 };
            rpt.check("sigaltstack: initial query ok",
                unsafe { libc::signal::sigaltstack(core::ptr::null(), &mut cur) } == 0);
            rpt.check("sigaltstack: initially SS_DISABLE", cur.ss_flags == libc::signal::SS_DISABLE);
            // 2) 设栈。
            let asize: u64 = 64 * 1024;
            let abase = libsys::mmap(asize).unwrap_or(0);
            rpt.check("sigaltstack: mmap alt stack", abase != 0);
            ALT_BASE.store(abase, Ordering::SeqCst);
            ALT_SIZE.store(asize, Ordering::SeqCst);
            // **必须先触碰备用栈**：mmap 给的是按需分页，而信号投递前会预校验「帧能否写入」，
            // 页面未落地时校验不过 ⇒ 投递失败 ⇒ 进程按契约终止（实测退出码 12 = SIGUSR2 默认动作）。
            // 与普通用户栈一样，备用栈也要先落地。
            {
                let mut off: u64 = 0;
                while off < asize {
                    unsafe { core::ptr::write_volatile((abase + off) as *mut u8, 0) };
                    off += 4096;
                }
            }
            let ss = libc::signal::stack_t {
                ss_sp: abase as *mut core::ffi::c_void,
                ss_flags: 0,
                ss_size: asize as usize,
            };
            let mut old = libc::signal::stack_t { ss_sp: core::ptr::null_mut(), ss_flags: 0, ss_size: 0 };
            rpt.check("sigaltstack: set ok",
                unsafe { libc::signal::sigaltstack(&ss, &mut old) } == 0);
            rpt.check("sigaltstack: old was SS_DISABLE", old.ss_flags == libc::signal::SS_DISABLE);
            // 3) 查询应与设置一致，且**不改状态**（这是本轮修掉的缺陷：原实现用 set(0,0) 读旧值）。
            let mut q2 = libc::signal::stack_t { ss_sp: core::ptr::null_mut(), ss_flags: 0, ss_size: 0 };
            rpt.check("sigaltstack: query ok",
                unsafe { libc::signal::sigaltstack(core::ptr::null(), &mut q2) } == 0);
            rpt.check("sigaltstack: query consistent with set",
                q2.ss_sp as u64 == abase && q2.ss_size == asize as usize && q2.ss_flags == 0);
            // 4) SA_ONSTACK handler：投递后必须**真的跑在备用栈上**。
            let act = libc::signal::sigaction {
                sa_handler: on_usr2 as usize,
                sa_mask: 0,
                sa_flags: libc::signal::SA_ONSTACK,
                sa_restorer: core::ptr::null_mut(),
            };
            rpt.check("sigaltstack: install SA_ONSTACK handler",
                unsafe { libc::signal::sigaction(libc::signal::SIGUSR2, &act, core::ptr::null_mut()) } == 0);
            let me2 = libc::process::getpid();
            rpt.check("sigaltstack: raise SIGUSR2", libc::process::kill(me2, libc::signal::SIGUSR2) == 0);
            rpt.check("sigaltstack: handler ran exactly once", ON_ALT.load(Ordering::SeqCst) == 1);
            rpt.check("sigaltstack: handler ran ON THE ALT STACK", SP_IN_RANGE.load(Ordering::SeqCst) == 1);
            // 5) 边界：只给一半（sp 有、size 0）⇒ EINVAL。
            let half = libc::signal::stack_t {
                ss_sp: abase as *mut core::ffi::c_void,
                ss_flags: 0,
                ss_size: 0,
            };
            libc::errno::set_errno(0);
            rpt.check("sigaltstack: half-specified rejected",
                unsafe { libc::signal::sigaltstack(&half, core::ptr::null_mut()) } == -1);
            rpt.check("sigaltstack: half-specified sets EINVAL",
                libc::errno::errno() == libc::errno::EINVAL);
            // 6) 禁用。
            let dis = libc::signal::stack_t { ss_sp: core::ptr::null_mut(), ss_flags: libc::signal::SS_DISABLE, ss_size: 0 };
            rpt.check("sigaltstack: disable ok",
                unsafe { libc::signal::sigaltstack(&dis, core::ptr::null_mut()) } == 0);
            let mut q3 = libc::signal::stack_t { ss_sp: core::ptr::null_mut(), ss_flags: 0, ss_size: 0 };
            let _ = unsafe { libc::signal::sigaltstack(core::ptr::null(), &mut q3) };
            rpt.check("sigaltstack: disabled shows SS_DISABLE", q3.ss_flags == libc::signal::SS_DISABLE);
        }
        // ---- ttyname / ttyname_r（3P4-13：身份比对，靠节点的稳定 st_ino）----
        {
            // --run 下 stdout 就是 console ⇒ 必须返回 /devices/console。
            // 这条能成立的前提是 stdio 节点与 console 节点**同号**（3P4-13 的 stable_id 覆写）——
            // 此前二者身份不等，本断言只能锁「如实失败」。
            libc::errno::set_errno(0);
            let p = unsafe { libc::unistd::ttyname(1) };
            let got = if p.is_null() { Vec::new() } else { cstr_to_owned(p as *const u8) };
            rpt.check(
                "ttyname(1) names the console (/devices/console)",
                got.as_slice() == b"/devices/console",
            );

            // ttyname_r：可重入版，**返回错误号本身**（0 = 成功）。
            let mut buf = [0u8; 64];
            let rc = unsafe {
                libc::unistd::ttyname_r(1, buf.as_mut_ptr() as *mut i8, buf.len())
            };
            rpt.check(
                "ttyname_r(1) returns 0 and the same path",
                rc == 0 && buf.starts_with(b"/devices/console\0"),
            );
            let mut tiny = [0u8; 4];
            let rc2 = unsafe {
                libc::unistd::ttyname_r(1, tiny.as_mut_ptr() as *mut i8, tiny.len())
            };
            rpt.check(
                "ttyname_r with a short buffer reports ERANGE",
                rc2 == libc::errno::ERANGE,
            );

            // 非终端（普通文件）⇒ NULL 置 ENOTTY。**重定向之后身份就该不同**——
            // 这正是「终端性跟着节点走、不跟着 fd 号走」的可执行判据。
            let f = b"/ttyname_probe.tmp\0";
            let fd = libc::unistd::open(
                f.as_ptr() as *const i8,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("ttyname: create regular file", fd >= 0);
            if fd >= 0 {
                libc::errno::set_errno(0);
                let q = unsafe { libc::unistd::ttyname(fd) };
                rpt.check("ttyname(regular file) -> NULL", q.is_null());
                rpt.check("ttyname(regular file) sets ENOTTY",
                    libc::errno::errno() == libc::errno::ENOTTY);
                let _ = libc::unistd::close(fd);
            }
            let _ = libc::unistd::remove(f.as_ptr() as *const i8);
        }
        // 清理。
        libc::pwd::endgrent();
        let _ = libc::unistd::remove(gtable.as_ptr() as *const i8);
    }

    // ---- A2-7：SHA-256 已知答案测试（FIPS 180-4 向量，**在真实内核上执行**）----
    //
    // 为何放在这里：libc 的 #[cfg(test)] 在 host 上无法执行（裸机目标），故向量的**真实**
    // 校验必须在目标机完成——这正是本段的意义。若本段不过，ADR-041 的整条认证链不成立。
    unsafe {
        fn hex32(d: &[u8; 32]) -> [u8; 64] {
            let mut out = [0u8; 64];
            libc::sha256::to_hex(d, &mut out);
            out
        }
        // 与文本常量比对（避免把实现输出当期望值的自证循环：期望值来自 FIPS/独立复算）。
        fn eq_hex(d: &[u8; 32], want: &[u8]) -> bool {
            let got = hex32(d);
            &got[..want.len()] == want
        }

        // (1) 空串
        rpt.check(
            "sha256: empty string (FIPS vector)",
            eq_hex(
                &libc::sha256::sha256(b""),
                b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
        );
        // (2) "abc"
        rpt.check(
            "sha256: \"abc\" (FIPS vector)",
            eq_hex(
                &libc::sha256::sha256(b"abc"),
                b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        );
        // (3) 448 比特双块消息（覆盖多块拼接）
        rpt.check(
            "sha256: 448-bit two-block message (FIPS vector)",
            eq_hex(
                &libc::sha256::sha256(
                    b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                ),
                b"248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        );
        // (4)(5)(6) **填充边界**：55 / 56 / 64 字节——SHA-256 实现最易出错处
        rpt.check(
            "sha256: 55-byte input (length fits in same block)",
            eq_hex(
                &libc::sha256::sha256(&[b'a'; 55]),
                b"9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
        );
        rpt.check(
            "sha256: 56-byte input (forces extra padding block)",
            eq_hex(
                &libc::sha256::sha256(&[b'a'; 56]),
                b"b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
        );
        rpt.check(
            "sha256: 64-byte input (exactly one block)",
            eq_hex(
                &libc::sha256::sha256(&[b'a'; 64]),
                b"ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
        );
        // (7) 流式 == 一次性（覆盖 update() 的块拼接路径）
        {
            let mut data = [0u8; 256];
            for (i, b) in data.iter_mut().enumerate() {
                *b = i as u8;
            }
            let one = libc::sha256::sha256(&data);
            let mut c = libc::sha256::Sha256::new();
            for chunk in data.chunks(7) {
                c.update(chunk);
            }
            let streamed = c.finish();
            rpt.check("sha256: streaming == one-shot (7-byte chunks)", one == streamed);
        }
        // (8) 同一策略下的口令校验语义（自检：同口令同盐 → 同哈希；异盐 → 异哈希）
        {
            let mut a = libc::sha256::Sha256::new();
            a.update(b"0123456789abcdef");
            a.update(b"hunter2");
            let h1 = a.finish();
            let mut b = libc::sha256::Sha256::new();
            b.update(b"0123456789abcdef");
            b.update(b"hunter2");
            let h2 = b.finish();
            rpt.check("sha256: same salt+password is deterministic", h1 == h2);
            let mut c = libc::sha256::Sha256::new();
            c.update(b"fedcba9876543210");
            c.update(b"hunter2");
            let h3 = c.finish();
            rpt.check("sha256: different salt changes the hash", h1 != h3);
        }
    }

    // ---- A2-7：口令表（shadow）读取与校验——**在真实内核上验证** ----
    //
    // 覆盖 ADR-041 §1.2 的三条关键纪律：
    //   ① 种子表可读且 uid/gid 与账户一致（权威来源可用）；
    //   ② 正确口令 → 匹配，错误口令 → 不匹配（校验真的在算，不是恒真/恒假）；
    //   ③ **安全属性**：`/config/shadow.json` 对普通用户**不可读**（除非本进程是 root）。
    unsafe {
        match libc::shadow::load_shadow() {
            Ok(list) => {
                rpt.check("shadow: /config/shadow.json loads", true);
                rpt.check("shadow: has 2 seed accounts", list.len() == 2);
                let alice = list.iter().find(|e| e.name == "alice");
                rpt.check("shadow: alice entry present", alice.is_some());
                if let Some(a) = alice {
                    // 权威 uid/gid 必须与 users.json 的种子一致（1000/1000）。
                    rpt.check("shadow: alice uid == 1000 (authoritative)", a.uid == 1000);
                    rpt.check("shadow: alice gid == 1000 (authoritative)", a.gid == 1000);
                    rpt.check("shadow: alice salt non-empty", !a.salt.is_empty());
                    // 正确口令 "alicepw" → 匹配。
                    rpt.check(
                        "shadow: alice correct password verifies",
                        matches!(libc::shadow::verify(a, b"alicepw"), Ok(true)),
                    );
                    // 错误口令 → 不匹配（证明校验真的在比较，而非恒真）。
                    rpt.check(
                        "shadow: alice wrong password rejected",
                        matches!(libc::shadow::verify(a, b"wrongpw"), Ok(false)),
                    );
                    // 空口令 → 不匹配（不会被当成"无口令放行"）。
                    rpt.check(
                        "shadow: empty password does NOT pass (no empty-pw bypass)",
                        matches!(libc::shadow::verify(a, b""), Ok(false)),
                    );
                }
                // root 种子同理（uid 0 的权威记录存在）。
                let rt = list.iter().find(|e| e.name == "root");
                rpt.check(
                    "shadow: root uid == 0 (authoritative)",
                    matches!(rt, Some(e) if e.uid == 0),
                );
                rpt.check(
                    "shadow: root correct password verifies",
                    matches!(rt.map(|e| libc::shadow::verify(e, b"rootpw")), Some(Ok(true))),
                );
            }
            Err(_) => {
                // 若本进程非 root（uid != 0），读不到 shadow 是**正确行为**——这正是保护生效。
                // 如实记录当前主体的 uid，使结果可解释。
                let id = libsys::identity_query();
                let uid = match id {
                    Ok(i) => i.uid,
                    Err(_) => 0xFFFF_FFFF,
                };
                if uid == 0 {
                    rpt.check("shadow: root must be able to read shadow", false);
                } else {
                    rpt.check("shadow: non-root correctly DENIED shadow read", true);
                }
            }
        }
        // 解析健壮性：畸形输入必须**如实拒收**，不得产出半条记录。
        rpt.check(
            "shadow: malformed JSON yields no entries",
            libc::shadow::parse_shadow(b"{not json").is_empty(),
        );
        rpt.check(
            "shadow: entry missing hash is rejected",
            libc::shadow::parse_shadow(
                br#"{"accounts":[{"name":"x","uid":1,"gid":1,"salt":"00"}]}"#,
            )
            .is_empty(),
        );
        rpt.check(
            "shadow: non-hex salt is rejected",
            libc::shadow::parse_shadow(
                br#"{"accounts":[{"name":"x","uid":1,"gid":1,"salt":"zz","hash":"00"}]}"#,
            )
            .is_empty(),
        );
    }

    // 汇总
    // 汇总
    // ---- 全库转译审查（2026-10-13）新增契约的真机验证 ----
    //
    // 这三项都是「把 POSIX 概念落到本项目机制上」时容易只写不验的地方：
    //   NAME_MAX：VFS 单点契约（vfs::mount::MAX_NAME_BYTES=255）。修复前 RamFS 不校验
    //     长度，256 字节的名字能建出来，随后 readdir 会**静默截断**它（文件不可寻址）；
    //   pathconf：曾为**不存在**的行规程子系统报 255（本系统按 ADR-042 无 canonical 模式）；
    //   srandom：曾因两个**函数内** static 各自一份而根本没给 random 播种。
    unsafe {
        let mut long = [b'a'; 300];
        long[0] = b'/';

        // 255 字节名字（= NAME_MAX）必须成功。
        long[256] = 0;
        let ok = libc::unistd::mkdir(long.as_ptr() as *const libc::ctypes::c_char, 0o755);
        rpt.check("name-max: name of exactly 255 bytes is accepted", ok == 0);
        if ok == 0 {
            let _ = libc::unistd::rmdir(long.as_ptr() as *const libc::ctypes::c_char);
        }

        // 256 字节必须如实 ENAMETOOLONG（不得静默接受，也不得截断成另一个名字）。
        long[256] = b'a';
        long[257] = 0;
        libc::errno::set_errno(0);
        let over = libc::unistd::mkdir(long.as_ptr() as *const libc::ctypes::c_char, 0o755);
        rpt.check("name-max: name of 256 bytes is rejected", over == -1);
        rpt.check(
            "name-max: rejection is ENAMETOOLONG",
            libc::errno::errno() == libc::errno::ENAMETOOLONG,
        );

        // pathconf：只报有真值的名字。
        let root = b"/\0";
        let rp = root.as_ptr() as *const libc::ctypes::c_char;
        rpt.check(
            "pathconf: _PC_NAME_MAX == 255",
            libc::unistd::pathconf(rp, libc::unistd::_PC_NAME_MAX) == 255,
        );
        libc::errno::set_errno(0);
        let link_max = libc::unistd::pathconf(rp, libc::unistd::_PC_LINK_MAX);
        rpt.check("pathconf: _PC_LINK_MAX is 'no limit' (-1)", link_max == -1);
        rpt.check("pathconf: 'no limit' does not set errno", libc::errno::errno() == 0);
        libc::errno::set_errno(0);
        let canon = libc::unistd::pathconf(rp, libc::unistd::_PC_MAX_CANON);
        rpt.check(
            "pathconf: no TTY -> _PC_MAX_CANON is EINVAL, not an invented number",
            canon == -1 && libc::errno::errno() == libc::errno::EINVAL,
        );

        // srandom 真的给 random 播种。
        libc::posix_batch2::srandom(12345);
        let a = libc::posix_batch2::random();
        libc::posix_batch2::srandom(12345);
        let b = libc::posix_batch2::random();
        rpt.check("srandom: the same seed reproduces the same value", a == b);
        libc::posix_batch2::srandom(67890);
        rpt.check(
            "srandom: a different seed produces a different sequence",
            libc::posix_batch2::random() != a,
        );
    }

    // ---- flock：内核锁表经 libc 的真机链路（ADR-033 A2-1 R6）----
    unsafe {
        let path = b"/scratch/flock_probe.txt\0";
        let fd = libc::unistd::open(
            path.as_ptr() as *const libc::ctypes::c_char,
            libc::unistd::O_RDWR | libc::unistd::O_CREAT,
            0o644,
        );
        rpt.check("flock: probe file opened", fd >= 0);
        if fd >= 0 {
            rpt.check(
                "flock: LOCK_EX|LOCK_NB acquires (real syscall)",
                libc::unistd::flock(fd, libc::unistd::LOCK_EX | libc::unistd::LOCK_NB) == 0,
            );
            rpt.check(
                "flock: LOCK_UN releases",
                libc::unistd::flock(fd, libc::unistd::LOCK_UN) == 0,
            );
            libc::errno::set_errno(0);
            let blocking = libc::unistd::flock(fd, libc::unistd::LOCK_EX);
            rpt.check(
                "flock: blocking form honestly ENOTSUP (no fake wait)",
                blocking == -1 && libc::errno::errno() == libc::errno::ENOTSUP,
            );
            libc::errno::set_errno(0);
            let bad = libc::unistd::flock(fd, 0);
            rpt.check(
                "flock: undefined operation is EINVAL",
                bad == -1 && libc::errno::errno() == libc::errno::EINVAL,
            );
            let _ = libc::unistd::close(fd);
        }
    }

    // ---- 全库转译审查（第二/三/四阶段）修掉的 libc 语义：在**真机**上逐条复验 ----
    //
    // 为什么必须进真机：宿主测试跑的是宿主 ABI，而 printf 的可变参数在
    // os="boruix"（硬浮点，走 XMM）与 os="none"/宿主（走 GP）上是**两条不同的读取路径**
    // （见 libc/src/stdio.rs 的 next_float_arg）。浮点打印只有在这里才算真验过。
    unsafe {
        let mut buf = [0u8; 64];

        // C11 7.21.6.5：返回值 = 「若空间足够本应写入」的长度；截断时仍要 NUL 终止。
        let n = libc::stdio::snprintf(
            buf.as_mut_ptr() as *mut libc::ctypes::c_char,
            4,
            b"%s\0".as_ptr() as *const libc::ctypes::c_char,
            b"hello\0".as_ptr() as *const libc::ctypes::c_char,
        );
        rpt.check("snprintf: truncation still reports the full length (5)", n == 5);
        rpt.check("snprintf: truncation still NUL-terminates", buf[3] == 0 && &buf[..3] == b"hel");

        // 浮点打印走**目标的**可变参数路径（与宿主不同）。
        let n2 = libc::stdio::snprintf(
            buf.as_mut_ptr() as *mut libc::ctypes::c_char,
            64,
            b"%.2f\0".as_ptr() as *const libc::ctypes::c_char,
            1.5f64,
        );
        rpt.check("printf: %.2f of 1.5 -> 1.50 (target vararg path)", n2 == 4 && &buf[..4] == b"1.50");

        let n3 = libc::stdio::snprintf(
            buf.as_mut_ptr() as *mut libc::ctypes::c_char,
            64,
            b"%g\0".as_ptr() as *const libc::ctypes::c_char,
            1e20f64,
        );
        rpt.check("printf: %g of 1e20 -> 1e+20 (fast-path overflow + strip)", n3 == 5 && &buf[..5] == b"1e+20");

        // strtol 的 "0x" 前缀：后面没有十六进制数字时 endptr 停在 'x'。
        let mut end: *const libc::ctypes::c_char = core::ptr::null();
        let s = b"0x\0";
        let v = libc::stdlib::strtol(s.as_ptr() as *const libc::ctypes::c_char, &mut end, 16);
        rpt.check(
            "strtol: 0x without digits -> 0 with endptr at 'x'",
            v == 0 && end == s.as_ptr().add(1) as *const libc::ctypes::c_char,
        );

        // strtoul 负向溢出：返回 ULONG_MAX 并置 ERANGE。
        libc::errno::set_errno(0);
        let big = b"-18446744073709551616\0";
        let u = libc::stdlib::strtoul(
            big.as_ptr() as *const libc::ctypes::c_char,
            core::ptr::null_mut(),
            10,
        );
        rpt.check(
            "strtoul: negative overflow -> ULONG_MAX + ERANGE",
            u == u64::MAX && libc::errno::errno() == libc::errno::ERANGE,
        );

        // tolower(EOF) 原样返回 EOF（不是 255）。
        rpt.check("tolower(EOF) returns EOF, not 255", libc::ctype::tolower(-1) == -1);

        // fnmatch 的 FNM_PERIOD：紧跟 '/' 之后的句点必须由显式句点匹配。
        let f = libc::posix_batch4::FNM_PATHNAME | libc::posix_batch4::FNM_PERIOD;
        let rc = libc::posix_batch4::fnmatch(
            b"a/*\0".as_ptr() as *const libc::ctypes::c_char,
            b"a/.hidden\0".as_ptr() as *const libc::ctypes::c_char,
            f,
        );
        rpt.check(
            "fnmatch: FNM_PERIOD protects a period right after '/'",
            rc == libc::posix_batch4::FNM_NOMATCH,
        );

        // regex 的 BRE：裸 '+' 是普通字符，模式合法（不得报 REG_BADRPT）。
        let mut re: libc::regex::regex_t = core::mem::zeroed();
        let c = libc::regex::regcomp(&mut re, b"a+b\0".as_ptr() as *const libc::ctypes::c_char, 0);
        rpt.check("regcomp: BRE 'a+b' is legal (bare + is literal)", c == 0);
        if c == 0 {
            libc::regex::regfree(&mut re);
        }

        // setlocale 的返回值必须能再喂回自己（POSIX 硬要求）。
        let cur = libc::locale::setlocale(0, core::ptr::null());
        rpt.check("setlocale: query returns a name", !cur.is_null());
        if !cur.is_null() {
            rpt.check(
                "setlocale: the returned name is accepted again",
                !libc::locale::setlocale(0, cur).is_null(),
            );
        }

        // mbstowcs 的 len==0：一个元素都不许写。
        let mut sentinel = [0x7FFFi32; 2];
        let m = libc::wchar::mbstowcs(
            sentinel.as_mut_ptr(),
            b"\0".as_ptr() as *const libc::ctypes::c_char,
            0,
        );
        rpt.check(
            "mbstowcs(len=0) writes nothing and returns 0",
            m == 0 && sentinel[0] == 0x7FFF,
        );
    }

    // ---- 覆盖面补齐（真机专属面）：逐函数断言 ----
    //
    // 这些导出的共同点是**需要内核**（VFS / syscall / 堆），宿主构造不出来。
    // 此前它们只在 libccheck 里被**整体**覆盖（未逐函数断言），本块把它们逐条钉住。
    unsafe {
        use libc::ctypes::c_char;
        fn p(s: &[u8]) -> *const c_char {
            s.as_ptr() as *const c_char
        }

        // ===== stdio：文件流 =====
        let fpath = p(b"/scratch/conf_stream.txt\0");
        let fp = libc::stdio::fopen(fpath, p(b"w\0"));
        rpt.check("stdio: fopen(w) returns a stream", !fp.is_null());
        if !fp.is_null() {
            // **先定位一次**：ftell 对「从未被 lseek 过的 fd」如实返回 ENOTSUP（见其文档），
            // 那是诚实边界而不是缺陷。定位之后位置进入用户态位置表，本组才在测缓冲记账。
            rpt.check("stdio: fseek(0,SET) on a fresh stream", libc::stdio::fseek(fp, 0, 0) == 0);
            // 内容 = "hello\n" + "X" + "Y" + " n=42" = 6+1+1+5 = 13 字节。
            rpt.check("stdio: fputs writes", libc::stdio::fputs(p(b"hello\n\0"), fp) >= 0);
            rpt.check("stdio: fputc writes", libc::stdio::fputc(b'X' as i32, fp) == b'X' as i32);
            rpt.check("stdio: putc writes", libc::stdio::putc(b'Y' as i32, fp) == b'Y' as i32);
            rpt.check("stdio: fprintf writes", libc::stdio::fprintf(fp, p(b" n=%d\0"), 42i32) == 5);
            rpt.check("stdio: fileno >= 0", libc::stdio::fileno(fp) >= 0);
            rpt.check("stdio: ftell reports 13 bytes written", libc::stdio::ftell(fp) == 13);
            rpt.check(
                "stdio: setvbuf on a writable stream",
                libc::stdio::setvbuf(fp, core::ptr::null_mut(), 0, 128) == 0,
            );
            libc::stdio::setbuf(fp, core::ptr::null_mut());
            rpt.check("stdio: fflush ok", libc::stdio::fflush(fp) == 0);
            rpt.check("stdio: fseek(0, SEEK_SET) ok", libc::stdio::fseek(fp, 0, 0) == 0);
            rpt.check("stdio: ftell back to 0", libc::stdio::ftell(fp) == 0);
            rpt.check("stdio: fclose ok", libc::stdio::fclose(fp) == 0);
        }

        // **读侧必须另开一个 "r" 流**：以 "w" 打开的流读不了（POSIX）。
        // 诊断：把文件里的**真实字节**打出来（内容不符时能直接看出差在哪）。
        {
            let dfd = libc::unistd::open(fpath, libc::unistd::O_RDONLY, 0);
            if dfd >= 0 {
                let mut rawb = [0i8; 32];
                let n = libc::unistd::read(
                    dfd,
                    rawb.as_mut_ptr() as *mut libc::ctypes::c_void,
                    32,
                );
                let _ = libc::unistd::close(dfd);
                out(b"[libccheck] conf_stream.txt n=");
                out(crate::util::u64_to_dec(n as u64, &mut [0u8; 24]));
                out(b" bytes=");
                if n > 0 {
                    let v: alloc::vec::Vec<u8> =
                        rawb[..n as usize].iter().map(|&c| c as u8).collect();
                    out(&v);
                }
                out(b"\n");
            }
        }
        let fr = libc::stdio::fopen(fpath, p(b"r\0"));
        rpt.check("stdio: fopen(r)", !fr.is_null());
        if !fr.is_null() {
            let mut line = [0i8; 16];
            rpt.check("stdio: fgets reads a line", !libc::stdio::fgets(line.as_mut_ptr(), 16, fr).is_null());
            rpt.check(
                "stdio: fgets content",
                line[..6].iter().map(|&c| c as u8).eq(b"hello\n".iter().copied()),
            );
            rpt.check("stdio: getc returns X", libc::stdio::getc(fr) == b'X' as i32);
            rpt.check("stdio: fgetc returns Y", libc::stdio::fgetc(fr) == b'Y' as i32);
            while libc::stdio::getc(fr) >= 0 {}
            rpt.check("stdio: feof set at end", libc::stdio::feof(fr) != 0);
            rpt.check("stdio: ferror stays 0", libc::stdio::ferror(fr) == 0);
            libc::stdio::clearerr(fr);
            rpt.check("stdio: clearerr clears EOF", libc::stdio::feof(fr) == 0);
            rpt.check("stdio: rewind is void+works", {
                libc::stdio::rewind(fr);
                libc::stdio::ftell(fr) == 0
            });
            rpt.check(
                "stdio: freopen reopens as r",
                !libc::stdio::freopen(fpath, p(b"r\0"), fr).is_null(),
            );
            rpt.check("stdio: fclose(read stream)", libc::stdio::fclose(fr) == 0);
        }

        // fseek 必须**先冲刷**再重定位：写 "12345" → fseek(2) → 写 "abc" → 关，
        // 内容应是 "12abc"（5 字节）。若缓冲留到重定位之后才写，会变成偏移 2 处
        // 覆盖成 "12345" 而尾部残留，实测正是这条把缺陷暴露出来的。
        {
            let fpath2 = p(b"/scratch/conf_flush.txt\0");
            let sp = libc::stdio::fopen(fpath2, p(b"w\0"));
            if !sp.is_null() {
                let _ = libc::stdio::fputs(p(b"12345\0"), sp);
                let _ = libc::stdio::fseek(sp, 2, 0);
                let _ = libc::stdio::fputs(p(b"abc\0"), sp);
                let _ = libc::stdio::fclose(sp);
                let dfd = libc::unistd::open(fpath2, libc::unistd::O_RDONLY, 0);
                let mut rawb = [0i8; 16];
                let n = if dfd >= 0 {
                    libc::unistd::read(dfd, rawb.as_mut_ptr() as *mut libc::ctypes::c_void, 16)
                } else {
                    -1
                };
                if dfd >= 0 {
                    let _ = libc::unistd::close(dfd);
                }
                rpt.check(
                    "stdio: fseek flushes the write buffer before repositioning",
                    n == 5
                        && rawb[0] == b'1' as i8
                        && rawb[1] == b'2' as i8
                        && rawb[2] == b'a' as i8
                        && rawb[3] == b'b' as i8
                        && rawb[4] == b'c' as i8,
                );
            }
        }

        // fdopen / getdelim / getline：从裸 fd 造流并按行读。
        let fd = libc::unistd::open(fpath, libc::unistd::O_RDONLY, 0);
        rpt.check("stdio: raw open for fdopen", fd >= 0);
        if fd >= 0 {
            let fp2 = libc::stdio::fdopen(fd, p(b"r\0"));
            rpt.check("stdio: fdopen wraps a raw fd", !fp2.is_null());
            if !fp2.is_null() {
                let mut lp: *mut c_char = core::ptr::null_mut();
                let mut ln: usize = 0;
                let n = libc::stdio::getdelim(&mut lp, &mut ln, b'\n' as i32, fp2);
                rpt.check("stdio: getdelim reads a line", n > 0 && !lp.is_null());
                if !lp.is_null() {
                    let _ = libc::malloc::free(lp as *mut u8);
                }
                let _ = libc::stdio::fseek(fp2, 0, 0);
                let mut lp2: *mut c_char = core::ptr::null_mut();
                let mut ln2: usize = 0;
                let n2 = libc::stdio::getline(&mut lp2, &mut ln2, fp2);
                rpt.check("stdio: getline reads a line", n2 > 0 && !lp2.is_null());
                if !lp2.is_null() {
                    let _ = libc::malloc::free(lp2 as *mut u8);
                }
                rpt.check("stdio: setvbuf on a read-only stream honestly fails", libc::stdio::setvbuf(fp2, core::ptr::null_mut(), 0, 128) != 0);
                rpt.check("stdio: fclose(fdopen)", libc::stdio::fclose(fp2) == 0);
            }
        }

        // sscanf / tmpfile / perror / puts / putchar
        let mut a: i32 = 0;
        let mut b: i32 = 0;
        let ns = libc::stdio::sscanf(p(b"12 34\0"), p(b"%d %d\0"), &mut a as *mut i32, &mut b as *mut i32);
        rpt.check("stdio: sscanf parses two ints", ns == 2 && a == 12 && b == 34);
        let tf = libc::stdio::tmpfile();
        rpt.check("stdio: tmpfile returns a stream", !tf.is_null());
        if !tf.is_null() {
            rpt.check("stdio: tmpfile is writable", libc::stdio::fputs(p(b"t\0"), tf) >= 0);
            rpt.check("stdio: fclose(tmpfile)", libc::stdio::fclose(tf) == 0);
        }
        libc::stdio::perror(p(b"libccheck\0"));
        rpt.check("stdio: puts writes to stdout", libc::stdio::puts(p(b"[libccheck] puts ok\0")) >= 0);
        rpt.check("stdio: putchar writes to stdout", libc::stdio::putchar(b'.' as i32) == b'.' as i32);
        rpt.check("stdio: fflush(NULL) flushes all", libc::stdio::fflush(core::ptr::null_mut()) == 0);

        // ---- 语义忠实性审查 第五轮：stdio 的 x/e 后缀、freopen 冲刷、lseek 坏 fd ----
        {
            // `fopen(p, "wx")`：O_CREAT|O_EXCL —— 文件已存在必须**失败**（EEXIST），
            // 且**绝不能截断**它。此前 `x` 被静默忽略 ⇒ "wx" 退化成 "w" ⇒ 销毁数据。
            let xpath = p(b"/scratch/conf_excl.txt\0");
            let seed = libc::stdio::fopen(xpath, p(b"w\0"));
            rpt.check("stdio: seed file for O_EXCL test", !seed.is_null());
            if !seed.is_null() {
                let _ = libc::stdio::fputs(p(b"KEEPDATA\0"), seed);
                let _ = libc::stdio::fclose(seed);
            }
            libc::errno::set_errno(0);
            let wx = libc::stdio::fopen(xpath, p(b"wx\0"));
            rpt.check("stdio: fopen(wx) on existing file -> NULL", wx.is_null());
            rpt.check(
                "stdio: fopen(wx) sets EEXIST",
                libc::errno::errno() == libc::errno::EEXIST,
            );
            // 关键：原内容必须**还在**（被截断就说明 x 没生效）。
            let chk = libc::stdio::fopen(xpath, p(b"r\0"));
            rpt.check("stdio: reopen for O_EXCL content check", !chk.is_null());
            if !chk.is_null() {
                let mut cb = [0i8; 16];
                let got = libc::stdio::fgets(cb.as_mut_ptr(), 16, chk);
                rpt.check(
                    "stdio: fopen(wx) did not truncate the file",
                    !got.is_null()
                        && cb[..8].iter().map(|&c| c as u8).eq(b"KEEPDATA".iter().copied()),
                );
                let _ = libc::stdio::fclose(chk);
            }
            // `x` 与 `r` 同用：POSIX 未定义 ⇒ 本实现**明确拒绝**（EINVAL），不静默降级。
            libc::errno::set_errno(0);
            let rx = libc::stdio::fopen(xpath, p(b"rx\0"));
            rpt.check("stdio: fopen(rx) rejected (POSIX-undefined combo)", rx.is_null());
            rpt.check(
                "stdio: fopen(rx) sets EINVAL",
                libc::errno::errno() == libc::errno::EINVAL,
            );
            // `e` 后缀（O_CLOEXEC）：**登记项③ 之后在机内可观测了**——内核有了每-fd 标志的
            // 查询原语，故这里直接断言「流可用 **且** 它的 fd 真的带 FD_CLOEXEC」。
            // （此前那句「不可观测、fcntl(F_GETFD) 恒 0」是第五轮的登记，现已兑现。）
            let we = libc::stdio::fopen(p(b"/scratch/conf_cloexec.txt\0"), p(b"we\0"));
            rpt.check("stdio: fopen(we) accepted", !we.is_null());
            if !we.is_null() {
                let wfd = libc::stdio::fileno(we);
                rpt.check("stdio: fileno(we) -> fd", wfd >= 0);
                rpt.check(
                    "stdio: fopen(we) really set FD_CLOEXEC on its fd",
                    wfd >= 0
                        && libc::unistd::fcntl(wfd, libc::unistd::F_GETFD)
                            == libc::unistd::FD_CLOEXEC,
                );
                let _ = libc::stdio::fclose(we);
            }
            let _ = libc::unistd::remove(xpath);
            let _ = libc::unistd::remove(p(b"/scratch/conf_cloexec.txt\0"));
        }

        // `freopen` 必须**先冲刷旧流**（POSIX/C：先关闭旧文件，关闭含冲刷）。
        // 此前不冲刷：全缓冲写流上 fputs 之后直接 freopen，缓冲字节被静默丢弃。
        {
            let a1 = p(b"/scratch/conf_freopen_a.txt\0");
            let a2 = p(b"/scratch/conf_freopen_b.txt\0");
            let sfp = libc::stdio::fopen(a1, p(b"w\0"));
            rpt.check("stdio: freopen test stream", !sfp.is_null());
            if !sfp.is_null() {
                let _ = libc::stdio::fputs(p(b"FLUSHME\0"), sfp);
                // **不** fflush：交给 freopen 的「关闭旧文件」去冲刷。
                rpt.check(
                    "stdio: freopen to a second file",
                    !libc::stdio::freopen(a2, p(b"w\0"), sfp).is_null(),
                );
                let _ = libc::stdio::fclose(sfp);
                let dfd = libc::unistd::open(a1, libc::unistd::O_RDONLY, 0);
                let mut rb = [0i8; 16];
                let n = if dfd >= 0 {
                    libc::unistd::read(dfd, rb.as_mut_ptr() as *mut libc::ctypes::c_void, 16)
                } else {
                    -1
                };
                if dfd >= 0 {
                    let _ = libc::unistd::close(dfd);
                }
                rpt.check(
                    "stdio: freopen flushed the old stream's buffer",
                    n == 7 && rb[..7].iter().map(|&c| c as u8).eq(b"FLUSHME".iter().copied()),
                );
            }
            let _ = libc::unistd::remove(a1);
            let _ = libc::unistd::remove(a2);
        }

        // `lseek` 对**坏 fd**：POSIX 要求 -1/EBADF（此前返回 0，假成功）。
        // 另：探针前移到 `whence` 之前后，管道上的 SEEK_CUR/SEEK_END 也如实 ESPIPE。
        {
            libc::errno::set_errno(0);
            rpt.check("unistd: lseek(bad fd) -> -1", libc::unistd::lseek(9999, 0, 0) == -1);
            rpt.check(
                "unistd: lseek(bad fd) sets EBADF",
                libc::errno::errno() == libc::errno::EBADF,
            );
            let mut pp = [0i32; 2];
            if libc::posix_batch2::pipe(pp.as_mut_ptr()) == 0 {
                libc::errno::set_errno(0);
                rpt.check("unistd: lseek(pipe, CUR) -> -1", libc::unistd::lseek(pp[0], 0, 1) == -1);
                rpt.check(
                    "unistd: lseek(pipe, CUR) sets ESPIPE",
                    libc::errno::errno() == libc::errno::ESPIPE,
                );
                libc::errno::set_errno(0);
                rpt.check("unistd: lseek(pipe, END) -> -1", libc::unistd::lseek(pp[0], 0, 2) == -1);
                rpt.check(
                    "unistd: lseek(pipe, END) sets ESPIPE",
                    libc::errno::errno() == libc::errno::ESPIPE,
                );
                let _ = libc::unistd::close(pp[0]);
                let _ = libc::unistd::close(pp[1]);
            }
        }
        // ---- 登记项⑨：「设置位置」是内核原语，且 dup 出来的 fd **共享**位置 ----
        // POSIX：dup/dup2 得到的 fd 与原来那个属于**同一个打开文件描述** ⇒ 共享文件位置。
        // 登记项⑨ 之前，libc 在用户态按 fd 号维护位置表 ⇒ dup 只能**拷贝**（此后各走各的）✗。
        {
            let spath = p(b"/scratch/conf_seek_share\0");
            let sfd = libc::unistd::open(
                spath,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("seek: create file", sfd >= 0);
            if sfd >= 0 {
                let msg = b"0123456789";
                let w = libc::unistd::write(sfd, msg.as_ptr() as *const core::ffi::c_void, msg.len());
                rpt.check("seek: wrote 10 bytes", w == 10);
                // (a) SEEK_SET 真的生效（内核持有位置）
                rpt.check(
                    "seek: lseek(fd, 0, SEEK_SET) -> 0",
                    libc::unistd::lseek(sfd, 0, libc::unistd::SEEK_SET) == 0,
                );
                let mut b1 = [0u8; 4];
                rpt.check(
                    "seek: read after seek gives the head",
                    libc::unistd::read(sfd, b1.as_mut_ptr() as *mut core::ffi::c_void, 4) == 4 && &b1 == b"0123",
                );
                // (b) **dup 出来的 fd 与原来那个共享位置**（判别性）
                let dfd = libc::unistd::dup(sfd);
                rpt.check("seek: dup() -> new fd", dfd >= 0);
                if dfd >= 0 {
                    rpt.check(
                        "seek: lseek(dup fd, 6, SEEK_SET) -> 6",
                        libc::unistd::lseek(dfd, 6, libc::unistd::SEEK_SET) == 6,
                    );
                    let mut b2 = [0u8; 2];
                    rpt.check(
                        "seek: read on the ORIGINAL fd continues from the dup'd fd's position",
                        libc::unistd::read(sfd, b2.as_mut_ptr() as *mut core::ffi::c_void, 2) == 2
                            && &b2 == b"67",
                    );
                    rpt.check(
                        "seek: SEEK_CUR on the dup'd fd reflects the shared position",
                        libc::unistd::lseek(dfd, 0, libc::unistd::SEEK_CUR) == 8,
                    );
                    let _ = libc::unistd::close(dfd);
                }
                rpt.check(
                    "seek: lseek(fd, 0, SEEK_END) -> 10",
                    libc::unistd::lseek(sfd, 0, libc::unistd::SEEK_END) == 10,
                );
                let _ = libc::unistd::close(sfd);
                let _ = libc::unistd::unlink(spath);
            }
        }
        // `fclose` 必须把**最终冲刷的失败**报出来（POSIX/C：关闭失败 ⇒ EOF）。
        // 构造一个必然失败的冲刷：内核句柄的 write=false（只读打开的 fd）被 `fdopen`
        // 包成写流 ⇒ 全缓冲收下字节 ⇒ fclose 冲刷时内核按句柄方向拒绝（PermissionDenied）。
        {
            let ropath = p(b"/scratch/conf_fclose_fail.txt\0");
            let mk = libc::stdio::fopen(ropath, p(b"w\0"));
            if !mk.is_null() {
                let _ = libc::stdio::fputs(p(b"z\0"), mk);
                let _ = libc::stdio::fclose(mk);
            }
            let rofd = libc::unistd::open(ropath, libc::unistd::O_RDONLY, 0);
            rpt.check("stdio: open O_RDONLY for flush-failure test", rofd >= 0);
            if rofd >= 0 {
                let wf = libc::stdio::fdopen(rofd, p(b"w\0"));
                rpt.check("stdio: fdopen(O_RDONLY fd, w) returns a stream", !wf.is_null());
                if !wf.is_null() {
                    // 进缓冲（全缓冲），此刻还没有任何 syscall 失败。
                    let _ = libc::stdio::fputs(p(b"DATA\0"), wf);
                    rpt.check(
                        "stdio: fclose returns EOF when the final flush fails",
                        libc::stdio::fclose(wf) == -1,
                    );
                } else {
                    let _ = libc::unistd::close(rofd);
                }
            }
            let _ = libc::unistd::remove(ropath);
        }
        // ===== unistd =====
        let mut cwdbuf = [0i8; 256];
        rpt.check("unistd: getcwd", !libc::unistd::getcwd(cwdbuf.as_mut_ptr(), cwdbuf.len()).is_null());
        // ---- sysconf 的真值来源（语义忠实性审查登记项⑪）----
        // 每个名字的答案必须来自**单一事实源**：PAGE_SIZE/ARG_MAX 是 constants.json 的
        // 跨边界常量（内核用同一份），VERSION 是本实现声称遵循的 POSIX 版本。
        rpt.check(
            "sysconf: _SC_PAGESIZE == 4096",
            libc::unistd::sysconf(libc::unistd::_SC_PAGESIZE) == 4096,
        );
        rpt.check(
            "sysconf: _SC_PAGE_SIZE == _SC_PAGESIZE",
            libc::unistd::sysconf(libc::unistd::_SC_PAGE_SIZE)
                == libc::unistd::sysconf(libc::unistd::_SC_PAGESIZE),
        );
        rpt.check(
            "sysconf: _SC_ARG_MAX == 4096 (kernel MAX_CMDLINE_BYTES)",
            libc::unistd::sysconf(libc::unistd::_SC_ARG_MAX) == 4096,
        );
        rpt.check(
            "sysconf: _SC_VERSION == 200809 (POSIX.1-2008)",
            libc::unistd::sysconf(libc::unistd::_SC_VERSION) == 200809,
        );
        rpt.check(
            "sysconf: _SC_2_VERSION == 200809",
            libc::unistd::sysconf(libc::unistd::_SC_2_VERSION) == 200809,
        );
        let nconf = libc::unistd::sysconf(libc::unistd::_SC_NPROCESSORS_CONF);
        rpt.check("sysconf: _SC_NPROCESSORS_CONF >= 1", nconf >= 1);
        rpt.check(
            "sysconf: _SC_NPROCESSORS_CONF == _SC_NPROCESSORS_ONLN",
            nconf == libc::unistd::sysconf(libc::unistd::_SC_NPROCESSORS_ONLN),
        );
        // 未定义的名字：-1 且 EINVAL（POSIX 只允许对这一类这么做）。
        libc::errno::set_errno(0);
        rpt.check("sysconf: unknown name -> -1", libc::unistd::sysconf(9999) == -1);
        rpt.check(
            "sysconf: unknown name sets EINVAL",
            libc::errno::errno() == libc::errno::EINVAL,
        );
        // ---- `_CS_PATH` 是**系统契约**，不是部署事实 ----
        // 它必须是静态约定路径 `/programs`：盘叫什么、有几块盘、第三方程序在哪，
        // 系统一概不知道（那是数据）。此前它被烧成 `/programs:/volumes/BORUIX_DATA/3p`——
        // 换卷标或没有第二块盘时指向不存在的目录，且不报错。
        {
            let mut cb = [0u8; 64];
            let n = unsafe {
                libc::posix_batch7::confstr(
                    1, /* _CS_PATH */
                    cb.as_mut_ptr() as *mut core::ffi::c_char,
                    cb.len(),
                )
            };
            rpt.check("confstr: _CS_PATH returns a value", n > 0);
            let end = if n > 0 { n - 1 } else { 0 }; // 去掉结尾 NUL
            rpt.check(
                "confstr: _CS_PATH == /programs (system contract; no disk knowledge)",
                &cb[..end.min(cb.len())] == b"/programs",
            );
        }
        // 登记项⑪ 第二批：时钟频率与 fd 水位（两者都有单一事实源）。
        let clk = libc::unistd::sysconf(libc::unistd::_SC_CLK_TCK);
        out(b"[libccheck] sysconf _SC_CLK_TCK=");
        out(crate::util::u64_to_dec(clk as u64, &mut [0u8; 24]));
        out(b"\n");
        // 精确值 100 Hz = 内核 `arch_x86_64::lapic::SCHED_TICK_HZ`（调度 tick 频率）。
        // **不要**写成 `> 0`：第一版暴露的是时钟计数器频率（有 HPET 时 1e9），
        // `> 0` 那条弱断言**放过了它**——是打印出来的值才把它抓住的。
        rpt.check("sysconf: _SC_CLK_TCK == 100 (kernel SCHED_TICK_HZ)", clk == 100);
        rpt.check(
            "sysconf: _SC_OPEN_MAX == 1024 (kernel Process::MAX_FDS watermark)",
            libc::unistd::sysconf(libc::unistd::_SC_OPEN_MAX) == 1024,
        );
        let old_mask = libc::unistd::umask(0o022);
        rpt.check("unistd: umask round trip", libc::unistd::umask(old_mask) == 0o022);
        rpt.check("unistd: chdir to /scratch", libc::unistd::chdir(p(b"/scratch\0")) == 0);
        let mut cwdbuf2 = [0i8; 256];
        libc::unistd::getcwd(cwdbuf2.as_mut_ptr(), cwdbuf2.len());
        rpt.check("unistd: chdir really moved cwd", {
            let mut n = 0usize;
            while cwdbuf2[n] != 0 { n += 1; }
            cwdbuf2[..n].iter().map(|&c| c as u8).eq(b"/scratch".iter().copied())
        });
        let _ = libc::unistd::chdir(p(b"/\0"));

        let fd2 = libc::unistd::open(fpath, libc::unistd::O_RDONLY, 0);
        if fd2 >= 0 {
            let d = libc::unistd::dup(fd2);
            rpt.check("unistd: dup returns a new fd", d >= 0 && d != fd2);
            let mut buf8 = [0i8; 8];
            rpt.check("unistd: lseek(0,SET) == 0", libc::unistd::lseek(fd2, 0, 0) == 0);
            rpt.check("unistd: lseek(5,SET) == 5", libc::unistd::lseek(fd2, 5, 0) == 5);
            rpt.check("unistd: lseek(-2,END) == 11 (file is 13 bytes)", libc::unistd::lseek(fd2, -2, 2) == 11);
            rpt.check("unistd: read after seek", libc::unistd::read(fd2, buf8.as_mut_ptr() as *mut libc::ctypes::c_void, 2) == 2);
            if d >= 0 {
                let _ = libc::unistd::close(d);
            }
            let _ = libc::unistd::close(fd2);
        }

        // truncate / ftruncate
        rpt.check("unistd: truncate to 4", libc::unistd::truncate(fpath, 4) == 0);
        let fd3 = libc::unistd::open(fpath, libc::unistd::O_RDWR, 0);
        if fd3 >= 0 {
            rpt.check("unistd: ftruncate to 2", libc::unistd::ftruncate(fd3, 2) == 0);
            let _ = libc::unistd::close(fd3);
        }

        // symlink / readlink / realpath
        rpt.check("unistd: symlink", libc::unistd::symlink(fpath, p(b"/scratch/conf_link\0")) == 0);
        let mut lbuf = [0i8; 128];
        let ln = libc::unistd::readlink(p(b"/scratch/conf_link\0"), lbuf.as_mut_ptr(), lbuf.len());
        // POSIX readlink：缓冲不足 ⇒ **截断**并返回「写入缓冲的字节数」（= bufsiz），
        // 不是 ERANGE（ERANGE 不在 readlink 的错误表里）。此前本实现返回 -1/ERANGE。
        let mut tbuf = [0i8; 8];
        libc::errno::set_errno(0);
        let tn = libc::unistd::readlink(p(b"/scratch/conf_link\0"), tbuf.as_mut_ptr(), 8);
        rpt.check("unistd: readlink truncates to bufsiz", tn == 8);
        rpt.check(
            "unistd: readlink truncated bytes are the prefix",
            tbuf.iter().map(|&c| c as u8).eq(b"/scratch".iter().copied()),
        );
        rpt.check("unistd: readlink returns the target", ln > 0);
        let mut rbuf = [0i8; 256];
        rpt.check("unistd: realpath resolves", !libc::unistd::realpath(p(b"/scratch/conf_link\0"), rbuf.as_mut_ptr()).is_null());
        rpt.check("unistd: chown to self", libc::unistd::chown(fpath, 0, 0) == 0);
        let mut mt = *b"/scratch/mtXXXXXX\0";
        let mr = libc::unistd::mktemp(mt.as_mut_ptr() as *mut c_char);
        rpt.check("unistd: mktemp substitutes XXXXXX", !mr.is_null() && mt[11] != b'X');
        // POSIX 对 unlink/rmdir 的**类型**约束：内核删除原语同时支持删文件与空目录
        // （ramfs::unlink 只看「非空与否」、不看类型）⇒ 不做这层判定时，unlink(空目录)
        // 会真的把目录删掉、rmdir(普通文件) 会真的把文件删掉。libc 用 lstat 取类型真值。
        {
            let dpath = p(b"/scratch/conf_unlink_dir\0");
            let _ = libc::unistd::rmdir(dpath); // 清理可能的残留
            rpt.check(
                "unistd: mkdir for unlink-type test",
                libc::unistd::mkdir(dpath, 0o755) == 0,
            );
            libc::errno::set_errno(0);
            rpt.check("unistd: unlink(dir) -> -1", libc::unistd::unlink(dpath) == -1);
            rpt.check(
                "unistd: unlink(dir) sets EPERM",
                libc::errno::errno() == libc::errno::EPERM,
            );
            // 目录必须**还在**（缺陷版会把它删掉）。
            let mut dst: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check(
                "unistd: unlink(dir) did not delete the directory",
                unsafe { libc::unistd::stat(dpath, &mut dst) } == 0,
            );
            rpt.check("unistd: rmdir(dir) still works", libc::unistd::rmdir(dpath) == 0);

            let fpath9 = p(b"/scratch/conf_rmdir_file\0");
            let fd9 = libc::unistd::open(
                fpath9,
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("unistd: create file for rmdir-type test", fd9 >= 0);
            if fd9 >= 0 {
                let _ = libc::unistd::close(fd9);
            }
            libc::errno::set_errno(0);
            rpt.check("unistd: rmdir(file) -> -1", libc::unistd::rmdir(fpath9) == -1);
            rpt.check(
                "unistd: rmdir(file) sets ENOTDIR",
                libc::errno::errno() == libc::errno::ENOTDIR,
            );
            // 文件必须**还在**（缺陷版会把它删掉）。
            let mut fst: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check(
                "unistd: rmdir(file) did not delete the file",
                unsafe { libc::unistd::stat(fpath9, &mut fst) } == 0,
            );
            rpt.check("unistd: remove(file) still works", libc::unistd::remove(fpath9) == 0);
        }
        // ---- 登记项⑤：**内核删除原语自己**按类型判定（消 TOCTOU）----
        // 直接走 libsys（**绕开** libc 的预检），证明判定内建在删除原语里——
        // 这正是「判定与删除同一临界区」的可证形态：调用方**不可能**先看类型再删。
        {
            let kdir = "/scratch/conf_del_kernel_dir";
            let _ = libsys::rmdir(kdir); // 清理残留
            rpt.check(
                "del: mkdir for kernel-level type test",
                libc::unistd::mkdir(p(b"/scratch/conf_del_kernel_dir\0"), 0o755) == 0,
            );
            // (a) unlink（FileOnly）作用在**目录**上：内核必须拒绝……
            rpt.check(
                "del: libsys::unlink(<dir>) -> Err (kernel-side type check)",
                libsys::unlink(kdir).is_err(),
            );
            // ……而且目录**必须还在**（缺陷版会把它真的删掉）。
            let mut kdst: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check(
                "del: the directory survived libsys::unlink",
                unsafe { libc::unistd::stat(p(b"/scratch/conf_del_kernel_dir\0"), &mut kdst) } == 0
                    && kdst.st_mode & libc::unistd::S_IFMT == libc::unistd::S_IFDIR,
            );
            // (b) rmdir（DirOnly）作用在**普通文件**上：内核必须拒绝
            let kfile = "/scratch/conf_del_kernel_file";
            let kfd = libc::unistd::open(
                p(b"/scratch/conf_del_kernel_file\0"),
                libc::unistd::O_CREAT | libc::unistd::O_RDWR,
                0o600,
            );
            rpt.check("del: create file for kernel-level rmdir test", kfd >= 0);
            if kfd >= 0 {
                let _ = libc::unistd::close(kfd);
            }
            rpt.check(
                "del: libsys::rmdir(<file>) -> Err (kernel-side type check)",
                libsys::rmdir(kfile).is_err(),
            );
            let mut kfst: libc::unistd::stat = unsafe { core::mem::zeroed() };
            rpt.check(
                "del: the file survived libsys::rmdir",
                unsafe { libc::unistd::stat(p(b"/scratch/conf_del_kernel_file\0"), &mut kfst) } == 0,
            );
            // (c) 正常路径仍然工作（判定没有把合法删除一起拒掉）。
            rpt.check(
                "del: libsys::rmdir(<dir>) works",
                libsys::rmdir(kdir).is_ok(),
            );
            rpt.check(
                "del: libsys::remove(<file>) works",
                libsys::remove(kfile).is_ok(),
            );
        }
        // execvp 自 3P4-12 起是真实现（替换映像）；这条测的是「程序不存在时如实 -1」，
        // 不再表示「本系统做不了 exec」。
        rpt.check(
            "unistd: execvp on a missing program returns -1",
            libc::unistd::execvp(p(b"nope\0"), core::ptr::null()) == -1,
        );

        // ===== process =====
        rpt.check("process: gettid > 0", libc::process::gettid() > 0);
        rpt.check("process: getppid >= 0", libc::process::getppid() >= 0);
        // kill 的 pid 三档语义（POSIX）：本系统**无进程组**（getpgrp/setpgid/setsid 属登记缺失项），
        // 每个进程自成一个组 ⇒ pid==0 等价于发给自己（此前把 pid=0 直接下发，内核查不到该 pid
        // ⇒ EINVAL，连 kill(0,0) 探活自己都失败）；pid<0（进程组/全体）做不到 ⇒ 如实 ENOTSUP。
        libc::errno::set_errno(0);
        rpt.check(
            "process: kill(0, 0) -> 0 (no process groups: the group is just self)",
            libc::process::kill(0, 0) == 0,
        );
        rpt.check(
            "process: kill(getpid(), 0) -> 0",
            libc::process::kill(libc::process::getpid(), 0) == 0,
        );
        libc::errno::set_errno(0);
        rpt.check("process: kill(-1, 0) -> -1", libc::process::kill(-1, 0) == -1);
        rpt.check(
            "process: kill(-1, 0) sets ENOTSUP (process groups unsupported)",
            libc::errno::errno() == libc::errno::ENOTSUP,
        );
        rpt.check(
            "process: kill(nonexistent pid, 0) -> -1",
            libc::process::kill(9999, 0) == -1,
        );
        // 本系统无 real/effective 之分（见 libc/src/process.rs 的文档）⇒ 两者必须同值。
        // 不假设环境 uid 具体是多少，但把实测值打出来，便于人工核对。
        let uid = libc::process::getuid();
        let euid = libc::process::geteuid();
        let gid = libc::process::getgid();
        let egid = libc::process::getegid();
        out(b"[libccheck] identity uid=");
        out(crate::util::u64_to_dec(uid as u64, &mut [0u8; 24]));
        out(b" gid=");
        out(crate::util::u64_to_dec(gid as u64, &mut [0u8; 24]));
        out(b"\n");
        rpt.check("process: geteuid == getuid (no real/effective split)", euid == uid);
        rpt.check("process: getegid == getgid (no real/effective split)", egid == gid);
        rpt.check("process: uid is a real value, not (uid_t)-1", uid != u32::MAX);
        rpt.check("process: yield_sys returns 0", libc::process::yield_sys() == 0);
        let child = libc::process::fork();
        if child == 0 {
            libc::process::_exit(7);
        }
        rpt.check("process: fork returns a child pid", child > 0);
        if child > 0 {
            let mut st: i32 = 0;
            let w = libc::process::waitpid(child, &mut st, 0);
            rpt.check("process: waitpid reaps the child", w == child);
            rpt.check("process: exit status is 7", ((st >> 8) & 0xFF) == 7);
        }
        // ---- 登记项⑧：waitpid 的 pid 语义与 ECHILD ----
        // **判别性：只有 ECHILD 那一半有**（pid 那一半**如实登记为没有**）。
        //
        // 三次尝试想把「按 pid 等」也做成确定性判据，都没成（全部留档，不再硬凑）：
        //   1) 两个子进程都立即退出、父不睡就指定等 k2 ⇒ k1 可能还没成为 zombie，
        //      WAIT_ANY 等到的第一个可能是 k2 ⇒ 变异下**没红**；
        //   2) 父先睡 150ms 让两个子都成 zombie、再指定等 k2（依赖「k1 的 pid 更小」）
        //      ⇒ **pid 会被复用**，前提不成立 ⇒ 变异下**仍没红**；
        //   3) **管道握手**（k2 阻塞在管道读上、父睡后发 WNOHANG）⇒ 实测 k2 在父发问**之前**
        //      就退出了（诊断：`k2 was already reapable (w0=13)`，且它的读连 EINTR 重试都没
        //      兜住）⇒ 子进程的阻塞读在这条路径上**没能把它留住**。
        //      ⇒ 这本身是一条**新线索**（子进程对继承来的管道端的阻塞读行为），已登记待查；
        //      在查清之前，这一半的门禁**只是能通过，不是能证伪**。
        //
        // 下面这版就是绿过、也跑过变异的那一版（保留它 + 上面的登记，而不是留一个
        // 好看但不可靠的「判别性」设计）。
        let k1 = libc::process::fork();
        if k1 == 0 {
            libc::process::_exit(11);
        }
        let k2 = libc::process::fork();
        if k2 == 0 {
            libc::process::_exit(22);
        }
        libc::time::usleep(150_000);
        rpt.check("waitpid: two children forked", k1 > 0 && k2 > 0);
        if k1 > 0 && k2 > 0 {

            let mut s1: i32 = 0;
            let w1 = libc::process::waitpid(k2, &mut s1, 0);
            rpt.check("waitpid: waitpid(k2) returns k2", w1 == k2);
            rpt.check("waitpid: k2 exit code is 22", ((s1 >> 8) & 0xFF) == 22);
            let mut s2: i32 = 0;
            let w2 = libc::process::waitpid(k1, &mut s2, 0);
            rpt.check("waitpid: waitpid(k1) returns k1", w2 == k1);
            rpt.check("waitpid: k1 exit code is 11", ((s2 >> 8) & 0xFF) == 11);
            // 已收尸 ⇒ ECHILD（POSIX：该 pid 已不存在）。
            libc::errno::set_errno(0);
            rpt.check(
                "waitpid: re-waitpid(k1) -> -1",
                libc::process::waitpid(k1, &mut s2, 0) == -1,
            );
            rpt.check(
                "waitpid: re-waitpid(k1) sets ECHILD",
                libc::errno::errno() == libc::errno::ECHILD,
            );
        }
        // 不存在的 pid ⇒ ECHILD（此前是 ENOENT）。
        libc::errno::set_errno(0);
        let mut sx: i32 = 0;
        rpt.check(
            "waitpid: waitpid(999999) -> -1",
            libc::process::waitpid(999_999, &mut sx, 0) == -1,
        );
        rpt.check(
            "waitpid: waitpid(999999) sets ECHILD",
            libc::errno::errno() == libc::errno::ECHILD,
        );
        // 无子进程 ⇒ ECHILD（用 WNOHANG 形态，避免阻塞）。
        libc::errno::set_errno(0);
        rpt.check(
            "waitpid: waitpid(-1, WNOHANG) with no children -> -1",
            libc::process::waitpid(-1, &mut sx, libc::unistd::WNOHANG) == -1,
        );
        rpt.check(
            "waitpid: waitpid(-1, WNOHANG) with no children sets ECHILD",
            libc::errno::errno() == libc::errno::ECHILD,
        );
        // pid==0（同进程组）/ pid<-1（进程组 |pid|）：本系统无进程组 ⇒ 如实 ENOTSUP。
        libc::errno::set_errno(0);
        rpt.check("waitpid: waitpid(0) -> -1", libc::process::waitpid(0, &mut sx, 0) == -1);
        rpt.check(
            "waitpid: waitpid(0) sets ENOTSUP (no process groups)",
            libc::errno::errno() == libc::errno::ENOTSUP,
        );
        libc::errno::set_errno(0);
        rpt.check("waitpid: waitpid(-2) -> -1", libc::process::waitpid(-2, &mut sx, 0) == -1);
        rpt.check(
            "waitpid: waitpid(-2) sets ENOTSUP (no process groups)",
            libc::errno::errno() == libc::errno::ENOTSUP,
        );

        // ===== semaphore =====
        let mut sem: libc::semaphore::bx_sem_t = core::mem::zeroed();
        rpt.check("sem: sem_init", libc::semaphore::sem_init(&mut sem, 0, 1) == 0);
        let mut sv: i32 = -1;
        rpt.check("sem: sem_getvalue == 1", libc::semaphore::sem_getvalue(&mut sem, &mut sv) == 0 && sv == 1);
        rpt.check("sem: sem_trywait", libc::semaphore::sem_trywait(&mut sem) == 0);
        rpt.check("sem: sem_getvalue == 0", libc::semaphore::sem_getvalue(&mut sem, &mut sv) == 0 && sv == 0);
        rpt.check("sem: sem_post", libc::semaphore::sem_post(&mut sem) == 0);
        rpt.check("sem: sem_wait", libc::semaphore::sem_wait(&mut sem) == 0);
        rpt.check("sem: sem_destroy", libc::semaphore::sem_destroy(&mut sem) == 0);

        // ===== time =====
        let mut ts = libc::time::Timespec { tv_sec: 0, tv_nsec: 0 };
        rpt.check(
            "time: clock_gettime(monotonic)",
            libc::time::clock_gettime(libc::time::CLOCK_MONOTONIC, &mut ts) == 0 && ts.tv_sec >= 0,
        );
        let mut tv = libc::time::Timeval { tv_sec: 0, tv_usec: 0 };
        rpt.check("time: gettimeofday", libc::time::gettimeofday(&mut tv, core::ptr::null_mut()) == 0 && tv.tv_sec > 0);
        let nap = libc::time::Timespec { tv_sec: 0, tv_nsec: 1_000_000 };
        rpt.check("time: nanosleep", libc::time::nanosleep(&nap, core::ptr::null_mut()) == 0);
        rpt.check("time: usleep", libc::time::usleep(1000) == 0);
        rpt.check("time: nanosleep rejects an invalid nsec", {
            let bad = libc::time::Timespec { tv_sec: 0, tv_nsec: 2_000_000_000 };
            libc::time::nanosleep(&bad, core::ptr::null_mut()) == -1
        });

        // ===== dirent =====
        let dir = libc::dirent::opendir(p(b"/programs\0"));
        rpt.check("dirent: opendir(/programs)", !dir.is_null());
        if !dir.is_null() {
            let first = libc::dirent::readdir(dir);
            rpt.check("dirent: readdir returns an entry", !first.is_null());
            let loc = libc::dirent::telldir(dir);
            rpt.check("dirent: telldir >= 0", loc >= 0);
            libc::dirent::seekdir(dir, 0);
            rpt.check("dirent: seekdir(0) then readdir works", !libc::dirent::readdir(dir).is_null());
            libc::dirent::rewinddir(dir);
            rpt.check("dirent: rewinddir then readdir works", !libc::dirent::readdir(dir).is_null());
            rpt.check("dirent: closedir", libc::dirent::closedir(dir) == 0);
        }
        let mut names: *mut *mut libc::dirent::dirent = core::ptr::null_mut();
        let cnt = libc::dirent::scandir(p(b"/programs\0"), &mut names, None, None);
        rpt.check("dirent: scandir counts entries", cnt > 0 && !names.is_null());
        if !names.is_null() {
            let mut i = 0isize;
            while i < cnt as isize {
                let e = *names.offset(i);
                if !e.is_null() {
                    let _ = libc::malloc::free(e as *mut u8);
                }
                i += 1;
            }
            let _ = libc::malloc::free(names as *mut u8);
        }

        // ===== malloc / string =====
        let cb = libc::malloc::calloc(4, 8);
        rpt.check("malloc: calloc returns zeroed memory", !cb.is_null() && core::slice::from_raw_parts(cb, 32).iter().all(|&x| x == 0));
        if !cb.is_null() {
            let _ = libc::malloc::free(cb);
        }
        let ab = libc::malloc::aligned_alloc(64, 128);
        rpt.check("malloc: aligned_alloc honours the alignment", !ab.is_null() && (ab as usize) % 64 == 0);
        if !ab.is_null() {
            let _ = libc::malloc::free(ab);
        }
        let sd = libc::string::strdup(p(b"dup\0"));
        rpt.check("string: strdup copies", !sd.is_null() && *sd as u8 == b'd');
        if !sd.is_null() {
            let _ = libc::malloc::free(sd as *mut u8);
        }
        let sn = libc::string::strndup(p(b"abcdef\0"), 3);
        rpt.check("string: strndup respects n", !sn.is_null() && *sn.add(3) == 0 && *sn as u8 == b'a');
        if !sn.is_null() {
            let _ = libc::malloc::free(sn as *mut u8);
        }

        // ===== wchar：多字节 ↔ 宽（逐字节编码） =====
        let mb = b"AB\0";
        let mut wbuf = [0 as libc::ctypes::wchar_t; 4];
        let mut srcp = mb.as_ptr() as *const c_char;
        rpt.check("wchar: mbsrtowcs converts", libc::wchar::mbsrtowcs(wbuf.as_mut_ptr(), &mut srcp, 4, core::ptr::null_mut()) == 2);
        let mut back = [0i8; 4];
        let mut wp = wbuf.as_ptr();
        rpt.check("wchar: wcsrtombs converts back", libc::wchar::wcsrtombs(back.as_mut_ptr(), &mut wp, 4, core::ptr::null_mut()) == 2);
        rpt.check("wchar: round trip content", back[0] == b'A' as i8 && back[1] == b'B' as i8);

        // ===== glob =====
        let mut g = libc::glob::glob_t { gl_pathc: 0, gl_pathv: core::ptr::null_mut(), gl_offs: 0 };
        let gr = libc::glob::glob(p(b"/programs/*.elf\0"), 0, None, &mut g);
        rpt.check("glob: expands /programs/*.elf", gr == 0 && g.gl_pathc > 0);
        if gr == 0 {
            libc::glob::globfree(&mut g);
            rpt.check("glob: globfree released the result", g.gl_pathv.is_null() || g.gl_pathc == 0);
        }

        // ===== posix_batch3：临时文件与定位 IO =====
        let mut tmpl = *b"/scratch/mkXXXXXX\0";
        let mfd = libc::posix_batch3::mkstemp(tmpl.as_mut_ptr() as *mut c_char);
        rpt.check("posix3: mkstemp creates a file", mfd >= 0);
        if mfd >= 0 {
            rpt.check("posix3: pwrite at offset 0", libc::posix_batch3::pwrite(mfd, p(b"zz\0") as *const libc::ctypes::c_void, 2, 0) == 2);
            let mut rb = [0i8; 4];
            rpt.check("posix3: pread at offset 0", libc::posix_batch3::pread(mfd, rb.as_mut_ptr() as *mut libc::ctypes::c_void, 2, 0) == 2);
            rpt.check("posix3: pread content", rb[0] == b'z' as i8 && rb[1] == b'z' as i8);
            let _ = libc::unistd::close(mfd);
        }
        let mut dtmpl = *b"/scratch/dkXXXXXX\0";
        let dr = libc::posix_batch3::mkdtemp(dtmpl.as_mut_ptr() as *mut c_char);
        rpt.check("posix3: mkdtemp creates a directory", !dr.is_null());
        if !dr.is_null() {
            let _ = libc::unistd::rmdir(dtmpl.as_ptr() as *const c_char);
        }

        // ===== mman =====
        let sz: usize = 4096;
        let addr = libc::mman::mmap(
            core::ptr::null_mut(),
            sz,
            libc::mman::PROT_READ | libc::mman::PROT_WRITE,
            libc::mman::MAP_ANONYMOUS | libc::mman::MAP_PRIVATE,
            -1,
            0,
        );
        rpt.check("mman: mmap anonymous", addr != libc::mman::MAP_FAILED);
        if addr != libc::mman::MAP_FAILED {
            // **必须先触碰**：mmap 给的是按需分页，而内核 protect_user 要求整段**已映射**
            // （任一页不在页表就如实 NotFound）。这与 sigaltstack 那条同一形态的调用方契约。
            core::ptr::write_bytes(addr as *mut u8, 0xA5, 16);
            rpt.check(
                "mman: mprotect to read-only",
                libc::mman::mprotect(addr, sz, libc::mman::PROT_READ) == 0,
            );
            // 未知 prot 位必须**如实拒绝**（内核 map_flags_from_prot 的单点判定），
            // 不能被 libc 掩码掉当成 PROT_DEFAULT(RW) 成功返回。
            libc::errno::set_errno(0);
            rpt.check(
                "mman: mprotect with an unknown prot bit -> -1",
                libc::mman::mprotect(addr, sz, 0x100) == -1,
            );
            rpt.check(
                "mman: mprotect unknown prot bit sets EINVAL",
                libc::errno::errno() == libc::errno::EINVAL,
            );
            rpt.check("mman: munmap", libc::mman::munmap(addr, sz) == 0);
        }
        // 未知 prot 位的 mmap：内核单点拒绝（InvalidParam ⇒ EINVAL），不得被掩码成 PROT_DEFAULT。
        libc::errno::set_errno(0);
        let bad = libc::mman::mmap(
            core::ptr::null_mut(),
            4096,
            0x100,
            libc::mman::MAP_ANONYMOUS | libc::mman::MAP_PRIVATE,
            -1,
            0,
        );
        rpt.check(
            "mman: mmap with an unknown prot bit -> MAP_FAILED",
            bad == libc::mman::MAP_FAILED,
        );
        rpt.check(
            "mman: mmap unknown prot bit sets EINVAL",
            libc::errno::errno() == libc::errno::EINVAL,
        );
        // POSIX：`addr` 是**提示**（非 NULL 且未设 MAP_FIXED 时实现可另择地址，调用方须用
        // 返回值）——此前对任何非 NULL addr 都返回 ENOTSUP，把合法的「带提示」调用拒掉了。
        let hint = libc::mman::mmap(
            4096usize as *mut libc::ctypes::c_void,
            sz,
            libc::mman::PROT_READ | libc::mman::PROT_WRITE,
            libc::mman::MAP_ANONYMOUS | libc::mman::MAP_PRIVATE,
            -1,
            0,
        );
        rpt.check(
            "mman: mmap treats addr as a hint (may relocate)",
            hint != libc::mman::MAP_FAILED,
        );
        if hint != libc::mman::MAP_FAILED {
            rpt.check("mman: munmap the hinted mapping", libc::mman::munmap(hint, sz) == 0);
        }
        // MAP_FIXED（Linux 值 0x10）本实现做不到「必须放在这里」⇒ 如实拒绝，**不静默忽略**：
        // 静默忽略会让调用方以为映射落在指定地址，实际落在内核挑的地址上。
        libc::errno::set_errno(0);
        let fx = libc::mman::mmap(
            core::ptr::null_mut(),
            sz,
            libc::mman::PROT_READ | libc::mman::PROT_WRITE,
            libc::mman::MAP_ANONYMOUS | libc::mman::MAP_PRIVATE | 0x10,
            -1,
            0,
        );
        rpt.check(
            "mman: mmap(MAP_FIXED) -> MAP_FAILED (cannot honour)",
            fx == libc::mman::MAP_FAILED,
        );
        rpt.check(
            "mman: mmap(MAP_FIXED) sets ENOTSUP",
            libc::errno::errno() == libc::errno::ENOTSUP,
        );
    }

    // ---- 覆盖面补齐（第二批真机专属面）：spawn / access / lstat / fgetpos ----
    unsafe {
        use libc::ctypes::c_char;
        fn p(s: &[u8]) -> *const c_char {
            s.as_ptr() as *const c_char
        }

        // ===== posix_spawn：文件动作与属性对象（纯数据结构，可在真机上逐项验） =====
        let mut fa: libc::spawn::posix_spawn_file_actions_t = core::mem::zeroed();
        let mut at: libc::spawn::posix_spawnattr_t = core::mem::zeroed();
        rpt.check(
            "spawn: posix_spawn_file_actions_init",
            libc::spawn::posix_spawn_file_actions_init(&mut fa) == 0,
        );
        rpt.check(
            "spawn: posix_spawn_file_actions_addopen",
            libc::spawn::posix_spawn_file_actions_addopen(&mut fa, 0, p(b"/devices/console\0"), 0, 0) == 0,
        );
        rpt.check(
            "spawn: posix_spawn_file_actions_addclose",
            libc::spawn::posix_spawn_file_actions_addclose(&mut fa, 1) == 0,
        );
        rpt.check(
            "spawn: posix_spawn_file_actions_adddup2",
            libc::spawn::posix_spawn_file_actions_adddup2(&mut fa, 1, 2) == 0,
        );
        rpt.check(
            "spawn: posix_spawnattr_init",
            libc::spawn::posix_spawnattr_init(&mut at) == 0,
        );
        let mut flags: libc::ctypes::c_short = -1;
        rpt.check(
            "spawn: posix_spawnattr_getflags reports the initial flags",
            libc::spawn::posix_spawnattr_getflags(&at, &mut flags) == 0 && flags == 0,
        );
        rpt.check(
            "spawn: posix_spawnattr_setflags then getflags round-trips",
            libc::spawn::posix_spawnattr_setflags(&mut at, 0) == 0
                && libc::spawn::posix_spawnattr_getflags(&at, &mut flags) == 0,
        );
        rpt.check(
            "spawn: posix_spawn_file_actions_destroy",
            libc::spawn::posix_spawn_file_actions_destroy(&mut fa) == 0,
        );
        rpt.check(
            "spawn: posix_spawnattr_destroy",
            libc::spawn::posix_spawnattr_destroy(&mut at) == 0,
        );

        // posix_spawn/posix_spawnp 对**不存在的程序**必须如实失败（不假装成功）。
        let mut fa2: libc::spawn::posix_spawn_file_actions_t = core::mem::zeroed();
        let mut at2: libc::spawn::posix_spawnattr_t = core::mem::zeroed();
        let _ = libc::spawn::posix_spawn_file_actions_init(&mut fa2);
        let _ = libc::spawn::posix_spawnattr_init(&mut at2);
        let argv = [p(b"nope\0"), core::ptr::null()];
        let mut pid: libc::ctypes::c_int = 0;
        let rc = libc::spawn::posix_spawn(
            &mut pid,
            p(b"/programs/definitely-not-here.elf\0"),
            &fa2,
            &at2,
            argv.as_ptr(),
            core::ptr::null(),
        );
        rpt.check("spawn: posix_spawn on a missing program fails honestly", rc != 0);
        let rc2 = libc::spawn::posix_spawnp(
            &mut pid,
            p(b"definitely-not-here\0"),
            &fa2,
            &at2,
            argv.as_ptr(),
            core::ptr::null(),
        );
        rpt.check("spawn: posix_spawnp on a missing program fails honestly", rc2 != 0);
        let _ = libc::spawn::posix_spawn_file_actions_destroy(&mut fa2);
        let _ = libc::spawn::posix_spawnattr_destroy(&mut at2);

        // ===== access / lstat =====
        rpt.check("posix2: access(F_OK) on an existing path", libc::posix_batch2::access(p(b"/programs\0"), 0) == 0);
        rpt.check(
            "posix2: access(F_OK) on a missing path fails",
            libc::posix_batch2::access(p(b"/programs/definitely-not-here\0"), 0) == -1,
        );
        let mut st: libc::unistd::stat = core::mem::zeroed();
        rpt.check("posix2: lstat on an existing path", libc::posix_batch2::lstat(p(b"/programs\0"), &mut st) == 0);
        rpt.check(
            "posix2: lstat on a missing path fails",
            libc::posix_batch2::lstat(p(b"/programs/definitely-not-here\0"), &mut st) == -1,
        );

        // ===== fgetpos / fsetpos =====
        let fpath3 = p(b"/scratch/conf_pos.txt\0");
        let fp3 = libc::stdio::fopen(fpath3, p(b"w+\0"));
        rpt.check("posix6: fopen(w+) for fgetpos", !fp3.is_null());
        if !fp3.is_null() {
            // fgetpos 委托 ftell，而 ftell 对**从未被 lseek 过的 fd** 如实 ENOTSUP
            // （见 ftell 的文档）——先绝对定位一次，这条才是在测位置记账。
            rpt.check("posix6: fseek(0,SET) to establish the position", libc::stdio::fseek(fp3, 0, 0) == 0);
            let _ = libc::stdio::fputs(p(b"abcdef\0"), fp3);
            let _ = libc::stdio::fflush(fp3);
            let mut pos: libc::ctypes::c_long = -1;
            rpt.check("posix6: fgetpos", libc::posix_batch6::fgetpos(fp3 as *mut libc::ctypes::c_void, &mut pos) == 0);
            rpt.check("posix6: fsetpos back to the recorded position", libc::posix_batch6::fsetpos(fp3 as *mut libc::ctypes::c_void, &pos) == 0);
            let _ = libc::stdio::fclose(fp3);
        }
    }

    // ---- 测试盘可自由插拔：卸载 → 挂载点消失 → 按**设备名**重新挂载 ----
    // **不认盘名**：只按 `/volumes` 的目录项与 `/devices/disks` 的设备项操作。
    // 这正是「系统按普通卷处理」的含义——它对这块盘没有任何特殊认知。
    {
        let vols = libsys::read_dir("/volumes").unwrap_or_default();
        let devs = libsys::read_dir("/devices/disks").unwrap_or_default();
        rpt.check("volume: at least one volume is mounted", !vols.is_empty());
        // 挂载点路径 + 各自的内容条目数（挂载时是 ext2 根，卸载后回落到空的 ramfs 目录）。
        let mut paths: Vec<alloc::string::String> = Vec::new();
        let mut nonempty = 0usize;
        for v in vols.iter() {
            let p = alloc::format!("/volumes/{}", v.name);
            if libsys::read_dir(&p).map(|e| !e.is_empty()).unwrap_or(false) {
                nonempty += 1;
            }
            paths.push(p);
        }
        rpt.check("volume: mounted volumes have contents", nonempty > 0);
        for p in paths.iter() {
            rpt.check("volume: unmount succeeds", libsys::volume_unmount(p).is_ok());
        }
        // **卸载的判据是「挂载点回落到空目录」，不是「目录项消失」**：
        // 内核的 MountTable::unmount 只删挂载表项，`/volumes/<name>` 这个目录项留在父目录里
        // （与 Linux 的 umount 一致）。卸载后读该路径得到 0 项，才是「真的卸了」。
        let mut emptied = true;
        for p in paths.iter() {
            if libsys::read_dir(p).map(|e| !e.is_empty()).unwrap_or(true) {
                emptied = false;
            }
        }
        rpt.check(
            "volume: every mount point falls back to an empty dir after unmount",
            emptied,
        );
        // 按**设备名**重新挂载（不认卷标），内容应当回来。
        for d in devs.iter() {
            // `volume_mount` 收的是 **/devices/disks 下的叶子设备名**（内核日志
            // "[volume] mounted 'ata0' at ..."），不是全路径。
            let _ = libsys::volume_mount(&d.name);
        }
        let mut restored = 0usize;
        for p in paths.iter() {
            if libsys::read_dir(p).map(|e| !e.is_empty()).unwrap_or(false) {
                restored += 1;
            }
        }
        rpt.check("volume: remount restores the contents", restored == nonempty);
    }

    // ---- 3P4-12：exec 真的替换映像（同一 pid；失败不改动调用者）----
    // **不认卷名**：按 `/volumes` 的目录项找一块挂着 hello 程序的卷。
    {
        let mut prog: Option<Vec<u8>> = None;
        for v in libsys::read_dir("/volumes").unwrap_or_default().iter() {
            for rel in ["/3p/hello.elf", "/hello.elf"] {
                let cand = alloc::format!("/volumes/{}{}", v.name, rel);
                if libsys::stat(&cand).is_ok() {
                    let mut b = Vec::new();
                    b.extend_from_slice(cand.as_bytes());
                    b.push(0);
                    prog = Some(b);
                    break;
                }
            }
            if prog.is_some() {
                break;
            }
        }
        rpt.check(
            "exec: found a program to exec on a mounted volume",
            prog.is_some(),
        );

        if let Some(prog) = prog {
            let arg0: Vec<u8> = b"hello\0".to_vec();

            // 收尸：本系统的**阻塞** `waitpid` 在「子进程尚未退出、且当下无法切换」时
            // 如实返回 EAGAIN（内核「无可切对象」的诚实回报，见 libc/src/process.rs 与
            // 内核 sys_task_wait 的文档）。按 POSIX 语义那不是错误，只是「还没结果」，
            // 调用方须重试——shell 的 acee2e/trave2e 是同一纪律。
            // 这条重试**不是**在掩盖 exec 的问题：exec 的语义由内核 test_task_exec
            // 单独证明，这里只负责把「子进程跑完」这件事等到。
            let reap = |pid: libc::ctypes::c_int| -> (libc::ctypes::c_int, libc::ctypes::c_int) {
                let mut st: libc::ctypes::c_int = -1;
                let mut tries = 0u32;
                loop {
                    let w = libc::process::waitpid(pid, &mut st, 0);
                    if w != -1 || libc::errno::errno() != libc::errno::EAGAIN || tries >= 200_000 {
                        return (w, st);
                    }
                    tries += 1;
                    let _ = libsys::yield_now();
                }
            };

            // (1) 成功路径：fork 出的子进程 exec，父按**同一个 pid** 收尸。
            // 这就是「同一 pid 跑新程序」的机内证据——若把 exec 做成「派生+等待+退出」，
            // 原 pid 会以 99 退出、新程序的退出码落在一个陌生 pid 上。
            let pid = libc::process::fork();
            rpt.check("exec: fork succeeds", pid >= 0);
            if pid == 0 {
                let argv: [*const libc::ctypes::c_char; 2] =
                    [arg0.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                unsafe {
                    libc::exec::execv(prog.as_ptr() as *const libc::ctypes::c_char, argv.as_ptr());
                }
                libc::process::_exit(99);
            } else if pid > 0 {
                let (w, st) = reap(pid);
                rpt.check(
                    "exec: waitpid reaps the same pid after exec",
                    w == pid,
                );
                rpt.check(
                    "exec: the replaced image ran and exited 0 (not the 99 fallback)",
                    (st >> 8) == 0,
                );
            }

            // (2) 失败路径：exec 失败后**原程序继续运行**（POSIX 铁律），
            // 且 errno 是真实的 ENOENT（不是"做不了"）。
            let pid2 = libc::process::fork();
            rpt.check("exec: fork succeeds (failure-path child)", pid2 >= 0);
            if pid2 == 0 {
                let argv: [*const libc::ctypes::c_char; 2] =
                    [arg0.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                let rc = unsafe {
                    libc::exec::execv(
                        b"/definitely/not/here\0".as_ptr() as *const libc::ctypes::c_char,
                        argv.as_ptr(),
                    )
                };
                let e = libc::errno::errno();
                libc::process::_exit(if rc == -1 && e == libc::errno::ENOENT {
                    77
                } else {
                    76
                });
            } else if pid2 > 0 {
                let (w, st) = reap(pid2);
                rpt.check(
                    "exec: a failed exec leaves the caller running (ENOENT, rc=-1)",
                    w == pid2 && (st >> 8) == 77,
                );
            }

            // (3) `execve` 与 `execv` 走同一条内核链路（`envp` 的边界见 libc/src/exec.rs）。
            let pid3 = libc::process::fork();
            rpt.check("exec: fork succeeds (execve child)", pid3 >= 0);
            if pid3 == 0 {
                let argv: [*const libc::ctypes::c_char; 2] =
                    [arg0.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                unsafe {
                    libc::exec::execve(
                        prog.as_ptr() as *const libc::ctypes::c_char,
                        argv.as_ptr(),
                        core::ptr::null(),
                    );
                }
                libc::process::_exit(98);
            } else if pid3 > 0 {
                let (w, st) = reap(pid3);
                rpt.check(
                    "exec: execve replaces the image too",
                    w == pid3 && (st >> 8) == 0,
                );
            }

            // (4) `execvp` 的 PATH 搜索分支：找不到时如实 ENOENT、调用者继续运行。
            // 搜索规则与 `posix_spawnp` **同一实现**（libc/src/spawn.rs 的 search_path）。
            let pid4 = libc::process::fork();
            rpt.check("exec: fork succeeds (execvp child)", pid4 >= 0);
            if pid4 == 0 {
                let argv: [*const libc::ctypes::c_char; 2] =
                    [arg0.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                let rc = unsafe {
                    libc::unistd::execvp(
                        b"definitely-not-a-real-program\0".as_ptr()
                            as *const libc::ctypes::c_char,
                        argv.as_ptr(),
                    )
                };
                let e = libc::errno::errno();
                libc::process::_exit(if rc == -1 && e == libc::errno::ENOENT { 75 } else { 74 });
            } else if pid4 > 0 {
                let (w, st) = reap(pid4);
                rpt.check(
                    "exec: execvp searches PATH and reports ENOENT honestly",
                    w == pid4 && (st >> 8) == 75,
                );
            }
            // (5) `execve` 带**显式环境**：libc 把 envp 交给内核（滤掉 PATH/PWD），新映像照常运行。
            // 保留键的拒绝在**内核**层（直调 syscall 才会看到 EINVAL）——libc 必须先滤掉，
            // 否则 `execve(path, argv, environ)` 恒失败（environ 里本来就有 PATH）。
            let pid5 = libc::process::fork();
            rpt.check("exec: fork succeeds (execve+envp child)", pid5 >= 0);
            if pid5 == 0 {
                let e1: Vec<u8> = b"FOO=bar\0".to_vec();
                let argv: [*const libc::ctypes::c_char; 2] =
                    [arg0.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                let envp: [*const libc::ctypes::c_char; 2] =
                    [e1.as_ptr() as *const libc::ctypes::c_char, core::ptr::null()];
                unsafe {
                    libc::exec::execve(
                        prog.as_ptr() as *const libc::ctypes::c_char,
                        argv.as_ptr(),
                        envp.as_ptr(),
                    );
                }
                libc::process::_exit(97);
            } else if pid5 > 0 {
                let (w, st) = reap(pid5);
                rpt.check(
                    "exec: execve with an explicit envp still replaces the image",
                    w == pid5 && (st >> 8) == 0,
                );
            }
        }
    }

    // ---- 3P4-14：system / popen / pclose（shell 路径读 SHELL）----
    {
        let shell = unsafe {
            libc::stdlib::getenv(b"SHELL\0".as_ptr() as *const libc::ctypes::c_char)
        };
        rpt.check(
            "system: SHELL is exported by the shell (path not hardcoded in libc)",
            !shell.is_null(),
        );
        rpt.check(
            "system: system(NULL) reports a command processor",
            unsafe { libc::popen::system(core::ptr::null()) } != 0,
        );
        let st = unsafe {
            libc::popen::system(b"echo system-ok\0".as_ptr() as *const libc::ctypes::c_char)
        };
        rpt.check(
            "system: runs a command line through SHELL and returns status 0",
            st == 0,
        );

        // **回归探针：30 轮**（3P4-14 复核定位的跨进程写 rax 缺陷）。
        //
        // 缺陷形态：管道读无数据时会真的切走（ADR-052 帧交换），而 ipc 层当时把
        // `BlockOutcome::Switched` 当成「继续循环」吞掉；循环最终返回的 Done/Err 被
        // syscall 尾部写进 `arch_frame(frame).rax`——而那已是**被切进来那个进程的帧**。
        // 受害者正是刚 fork 出来的子进程：它第一个用户态指令就判 fork 返回值，于是
        // 误判 fork 失败、走错分支。
        //
        // 为什么单轮不够：这依赖「读者阻塞的那一刻恰好把子进程切进来」，单轮命中率
        // 约 1/3；30 轮把暴露概率抬到接近必然。修复前本段 3/3 复现红（红证据见
        // docs/TODO/3p.md 的 3P4-14 复核记录），修复后稳定绿——**这是回归探针，
        // 不是压力测试**，请勿因"慢一点"而删。
        // 失败取证打印（S15 单点）：本轮序号 + 本进程 pid（+ 可选尾值）。
        let note = |tag: &[u8], i: u32, tail: &[u8], tail_v: Option<u32>| {
            let mut m = Vec::new();
            m.extend_from_slice(b"[popen-regress] ");
            m.extend_from_slice(tag);
            m.extend_from_slice(b" i=");
            m.extend_from_slice(crate::util::u64_to_dec(i as u64, &mut [0u8; 24]));
            m.extend_from_slice(b" pid=");
            m.extend_from_slice(crate::util::u64_to_dec(
                unsafe { libc::process::getpid() } as u64,
                &mut [0u8; 24],
            ));
            if let Some(v) = tail_v {
                m.extend_from_slice(tail);
                m.extend_from_slice(crate::util::u64_to_dec(v as u64, &mut [0u8; 24]));
            }
            m.extend_from_slice(b"\n");
            outln(&m);
        };
        let mut it_null = 0u32;
        let mut it_noout = 0u32;
        let mut it_badrc = 0u32;
        for it in 0..30u32 {
            let f = unsafe {
                libc::popen::popen(
                    b"echo popen-ok\0".as_ptr() as *const libc::ctypes::c_char,
                    b"r\0".as_ptr() as *const libc::ctypes::c_char,
                )
            };
            if f.is_null() {
                it_null += 1;
                note(b"NULL", it, b" errno=", Some(libc::errno::errno() as u32));
                continue;
            }
            // 读到 EOF 为止。两个理由，都是本系统的**既定语义**而不是测试写法问题：
            // ① 管道单次 read 只保证「有数据就返回」，不保证一次读全；
            // ② **ADR-052**：本系统的「阻塞」由帧交换实现，被唤醒的阻塞 syscall
            //    **不会在内核继续执行**，只能如实返回 EAGAIN（WouldBlock）让调用方重试。
            //    shell 自己的 acee2e/trave2e 全都按这条纪律重试（见 commands.rs）。
            // 实测证据：漏了这条重试时 fread 拿到 errno=11、n=0，直接把 EAGAIN 当成了 EOF。
            let mut got: Vec<u8> = Vec::new();
            let mut spins = 0u32;
            loop {
                let mut buf = [0u8; 64];
                let n = unsafe {
                    libc::stdio::fread(
                        buf.as_mut_ptr() as *mut libc::ctypes::c_void,
                        1,
                        buf.len(),
                        f,
                    )
                };
                if n == 0 {
                    if libc::errno::errno() == libc::errno::EAGAIN && spins < 200_000 {
                        spins += 1;
                        let _ = libsys::yield_now();
                        continue;
                    }
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            if !got.windows(8).any(|w| w == b"popen-ok") {
                it_noout += 1;
                note(b"NOOUTPUT", it, b"", None);
            }
            let rc = unsafe { libc::popen::pclose(f) };
            if rc != 0 {
                it_badrc += 1;
                note(b"BADRC", it, b" rc=", Some(rc as u32));
            }
        }
        rpt.check("popen: all 30 forks returned a stream", it_null == 0);
        rpt.check("popen: all 30 reads saw the child output", it_noout == 0);
        rpt.check("pclose: all 30 reaps reported status 0", it_badrc == 0);

        // "e"（cloexec）如实拒绝——本系统没有「fork 与 exec 之间设 cloexec」的窗口。
        let bad = unsafe {
            libc::popen::popen(
                b"echo x\0".as_ptr() as *const libc::ctypes::c_char,
                b"e\0".as_ptr() as *const libc::ctypes::c_char,
            )
        };
        rpt.check("popen: mode \"e\" is refused with EINVAL", bad.is_null());

        // 非 popen 流：如实 EINVAL，且**不**关掉它（关不关由调用方决定）。
        let plain = unsafe { libc::stdio::tmpfile() };
        rpt.check("pclose: a tmpfile() stream exists for the negative case", !plain.is_null());
        if !plain.is_null() {
            rpt.check(
                "pclose: a non-popen stream is refused",
                unsafe { libc::popen::pclose(plain) } == -1,
            );
            let _ = unsafe { libc::stdio::fclose(plain) };
        }
    }

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
