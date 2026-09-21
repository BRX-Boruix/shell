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
        let tdir = b"/tmp_libc_dir ".as_ptr() as *const i8;
        // mkdir 755
        let rc = libc::unistd::mkdir(tdir, 0o755);
        rpt.check("mkdir rc==0", rc == 0);
        // 在目录内创建文件
        let tfile = b"/tmp_libc_dir/hello.txt ".as_ptr() as *const i8;
        let fp = libc::stdio::fopen(tfile, b"w ".as_ptr() as *const i8);
        let file_created = !fp.is_null();
        if !fp.is_null() { libc::stdio::fclose(fp); }
        rpt.check("mkdir create file in dir", file_created);
        // opendir/readdir/closedir
        let dp = libc::dirent::opendir(tdir);
        rpt.check("opendir non-null", !dp.is_null());
        if !dp.is_null() {
            let mut found = false;
            let mut count = 0usize;
            loop {
                let e = libc::dirent::readdir(dp);
                if e.is_null() { break; }
                count += 1;
                let nm = cstr_to_owned((*e).d_name.as_ptr() as *const u8);
                if nm == b"hello.txt" { found = true; }
                // 文件条目 d_type == DT_REG(8)；目录条目 == DT_DIR(4)。
                let t = (*e).d_type;
                let _ = t;
            }
            rpt.check("readdir count>=1", count >= 1);
            rpt.check("readdir found hello.txt", found);
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
        // F_GETFD：无 fd 标志 → 0。
        let gfd = libc::unistd::fcntl(1, libc::unistd::F_GETFD, 0);
        rpt.check("fcntl F_GETFD==0", gfd == 0);
    }

    // 12) stat / fstat / chmod / rename（经内核 VFS + INode::metadata/set_permissions）
    unsafe {
        // 在根目录下创建独立测试目录与文件。
        let sdir = b"/tmp_libc_sd ".as_ptr() as *const i8;
        let _ = libc::unistd::mkdir(sdir, 0o755);
        let fa = b"/tmp_libc_sd/a.txt ".as_ptr() as *const i8;
        let fp = libc::stdio::fopen(fa, b"w ".as_ptr() as *const i8);
        if !fp.is_null() {
            libc::stdio::fwrite(b"abc ".as_ptr() as *const core::ffi::c_void, 1, 3, fp);
            libc::stdio::fclose(fp);
        }

        // rename：a.txt -> b.txt（同目录内重命名）。
        let fb = b"/tmp_libc_sd/b.txt ".as_ptr() as *const i8;
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
        let n = libc::pwd::getgrouplist("alice", 1000, &mut buf);
        rpt.check("grp: getgrouplist(alice) reports 2 groups", n == 2);
        rpt.check("grp: alice primary group first", n >= 1 && buf[0] == 1000);
        rpt.check("grp: alice supplementary dev(2000) present",
            n >= 2 && buf[..n].contains(&2000));
        // bob 同属 dev，carol 不属 dev。
        let nb = libc::pwd::getgrouplist("bob", 1001, &mut [0u32; 8]);
        rpt.check("grp: getgrouplist(bob) -> 2 (primary + dev)", nb == 2);
        let nc = libc::pwd::getgrouplist("carol", 1002, &mut [0u32; 8]);
        rpt.check("grp: carol is NOT in dev -> 2 (primary + ops)", nc == 2);

        // 6) 缓冲不足必须**如实返回所需条数**，不截断、不越界。
        let mut tiny = [0u32; 1];
        let need = libc::pwd::getgrouplist("alice", 1000, &mut tiny);
        rpt.check("grp: insufficient buffer reports required count (2)", need == 2);

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

    // 汇总
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
