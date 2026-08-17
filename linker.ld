/* BORUIX 用户态程序链接脚本（仿 init）。
 * 用显式 `. = ALIGN(0x1000)` 使每个输出段起始地址 4K 页对齐
 * （内核 elf.rs 加载器要求 p_vaddr 页对齐），生成
 * text(RX) / rodata(R) / data+bss(RW) 多个 PT_LOAD 段，符合 W^X。
 */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : {
        *(.text .text.*)
    }

    . = ALIGN(0x1000);

    .rodata : {
        *(.rodata .rodata.*)
    }

    . = ALIGN(0x1000);

    .data : {
        *(.data .data.*)
    }

    . = ALIGN(0x1000);

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.interp)
        *(.dynsym)
        *(.dynstr)
        *(.dynamic)
        *(.gnu.hash)
        *(.hash)
        *(.gnu.version)
        *(.gnu.version_r)
        *(.gnu.version_d)
        *(.rela.dyn)
        *(.rela.plt)
        *(.rel.dyn)
        *(.rel.plt)
        *(.got)
        *(.got.plt)
    }
}
