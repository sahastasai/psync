MEMORY {
  FLASH : ORIGIN = 0x10000000, LENGTH = 4M
  RAM : ORIGIN = 0x20000000, LENGTH = 520K
}
/* cortex-m-rt places this block immediately after the vector table. */
SECTIONS {
  .start_block : ALIGN(4) { KEEP(*(.start_block)); } > FLASH
} INSERT AFTER .vector_table;
_stext = ADDR(.start_block) + SIZEOF(.start_block);
/* Heap follows all statics; reserve the upper 32 KiB for the stack. */
_heap_start = ALIGN(__euninit, 8);
_heap_end = ORIGIN(RAM) + LENGTH(RAM) - 32K;
ASSERT(_heap_end > _heap_start, "Not enough RAM for heap and stack");
