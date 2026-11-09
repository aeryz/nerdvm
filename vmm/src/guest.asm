bits 64
default abs
org 0x1000

; Store eight ASCII characters: ABCDEFGH.
mov rax, 0x4847464544434241
mov [0x8000], rax

; Read the complete 64-bit value back.
mov rax, [0x8000]

; Output one character at a time.
mov dx, 0x3f8
mov ecx, 8

.print:
    out dx, al
    shr rax, 8
    loop .print

hlt
