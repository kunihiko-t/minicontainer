OUTPUT_ARCH(riscv)
ENTRY(_start)

/* user imageは0x0010_0000から置き、user stack (0x3fff_0000..0x4000_0000)
   とguard page (0x3ffe_f000) へ届かない小ささに保つ。権限の違うsegment
   がpageを共有するとloaderに拒否されるため、section間にpage境界を置く。 */
BASE = 0x00100000;

SECTIONS
{
    . = BASE;
    .text : {
        *(.text.start)
        *(.text .text.*)
    }
    . = ALIGN(0x1000);
    .rodata : {
        *(.rodata .rodata.*)
    }
    . = ALIGN(0x1000);
    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
    }
    /* panic=abortのguestにunwind情報は要らない。loaderの権限検査を通す
       ため、置き場を持たないsectionは明示的に捨てる。 */
    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
    }
}
