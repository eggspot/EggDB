MEMORY
{
  FLASH (rx)  : ORIGIN = 0x00000000, LENGTH = 4M
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 4M
}

ENTRY(reset);

SECTIONS
{
  .vector_table ORIGIN(FLASH) :
  {
    LONG(ORIGIN(RAM) + LENGTH(RAM));
    KEEP(*(.vector_table.reset));
    KEEP(*(.vector_table.exceptions));
  } > FLASH

  .text :
  {
    *(.text .text.*);
  } > FLASH

  .rodata :
  {
    *(.rodata .rodata.*);
  } > FLASH

  .data : AT(ADDR(.rodata) + SIZEOF(.rodata))
  {
    __data_start = .;
    *(.data .data.*);
    . = ALIGN(4);
    __data_end = .;
  } > RAM

  __data_load = LOADADDR(.data);

  .bss (NOLOAD) :
  {
    __bss_start = .;
    *(.bss .bss.*);
    *(COMMON);
    . = ALIGN(4);
    __bss_end = .;
  } > RAM

  /DISCARD/ :
  {
    *(.ARM.exidx .ARM.exidx.*);
  }
}
