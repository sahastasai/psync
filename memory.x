MEMORY
{
  /* Standard RP2350 Flash boot slot (XIP cache window) assuming a 4MB flash chip */
  FLASH : ORIGIN = 0x10000000, LENGTH = 4M

  /* Total 520 KB contiguous SRAM available on the RP2350 */
  RAM   : ORIGIN = 0x20000000, LENGTH = 520K
}

/* Specify where the execution stack ends (grows downwards from high memory to low memory) */
_stack_start = ORIGIN(RAM) + LENGTH(RAM);

/* Define heap boundaries dynamically based on remaining free memory */
SECTIONS
{
  .heap (NOLOAD) :
  {
    . = ALIGN(8);
    _heap_start = .;
    
    /* Allocates all remaining RAM up to 32KB below the stack pointer to prevent collision */
    . = ORIGIN(RAM) + LENGTH(RAM) - 32K;
    
    . = ALIGN(8);
    _heap_end = .;
  } > RAM
}

