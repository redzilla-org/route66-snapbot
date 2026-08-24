// The PNG filter kernels are assembly because Go does not expose SIMD
// intrinsics. They implement the same byte-exact predictors as fastpng.go and
// operate only on validated, equal-length scanline allocations.

#include "textflag.h"

// func filterUpAVX2(current, previous unsafe.Pointer, length uintptr)
// Up has no horizontal dependency, so 32 bytes can be reconstructed at once
// with wrapping byte addition. The scalar tail handles arbitrary row widths.
TEXT ·filterUpAVX2(SB), NOSPLIT, $0-24
	MOVQ current+0(FP), DI
	MOVQ previous+8(FP), SI
	MOVQ length+16(FP), CX

up_vector:
	CMPQ CX, $32
	JB up_tail
	VMOVDQU (DI), Y0
	VPADDB (SI), Y0, Y0
	VMOVDQU Y0, (DI)
	ADDQ $32, DI
	ADDQ $32, SI
	SUBQ $32, CX
	JMP up_vector

up_tail:
	VZEROUPPER
	TESTQ CX, CX
	JZ up_done
up_tail_loop:
	MOVBLZX (DI), AX
	MOVBLZX (SI), BX
	ADDL BX, AX
	MOVB AX, (DI)
	INCQ DI
	INCQ SI
	DECQ CX
	JNZ up_tail_loop
up_done:
	RET

// func filterPaethRGBSSE41(current, previous unsafe.Pointer, pixels uintptr)
// Three RGB channels occupy independent 16-bit SIMD lanes. Processing one
// complete pixel per iteration preserves Paeth's horizontal dependency while
// calculating all channel predictors in parallel. Tie order is left, up, then
// upper-left, exactly as required by PNG.
TEXT ·filterPaethRGBSSE41(SB), NOSPLIT, $0-24
	MOVQ current+0(FP), DI
	MOVQ previous+8(FP), SI
	MOVQ pixels+16(FP), CX
	PXOR X15, X15              // Constant zero and byte-unpack high lanes.
	PXOR X8, X8                // A: reconstructed pixel immediately to the left.
	PXOR X9, X9                // C: previous-row pixel immediately upper-left.

paeth_loop:
	TESTQ CX, CX
	JZ paeth_done

	// B is the previous-row RGB triple, unpacked to three unsigned words.
	MOVL (SI), AX
	ANDL $0x00ffffff, AX
	MOVD AX, X2
	PUNPCKLBW X15, X2

	// Distances are |B-C|, |A-C| and |(B-C)+(A-C)| in signed words.
	MOVO X2, X3
	PSUBW X9, X3
	MOVO X3, X5
	PABSW X3, X3
	MOVO X8, X4
	PSUBW X9, X4
	PADDSW X4, X5
	PABSW X4, X4
	PABSW X5, X5

	// Find the minimum distance, then blend C/B followed by A. Applying A last
	// gives left predictor priority on ties; B already has priority over C.
	MOVO X3, X6
	PMINUW X4, X6
	PMINUW X5, X6
	MOVO X9, X10
	MOVO X6, X11
	PCMPEQW X4, X11
	MOVO X11, X0
	PBLENDVB X0, X2, X10
	MOVO X6, X11
	PCMPEQW X3, X11
	MOVO X11, X0
	PBLENDVB X0, X8, X10

	// Add the predictor modulo 256, retain reconstructed A/C state, and write
	// exactly three bytes so the final pixel never crosses the row allocation.
	MOVL (DI), AX
	ANDL $0x00ffffff, AX
	MOVD AX, X12
	PUNPCKLBW X15, X12
	PADDB X10, X12
	MOVO X12, X8
	MOVO X2, X9
	PACKUSWB X12, X12
	MOVD X12, AX
	MOVW AX, (DI)
	SHRL $16, AX
	MOVB AX, 2(DI)

	ADDQ $3, DI
	ADDQ $3, SI
	DECQ CX
	JMP paeth_loop

paeth_done:
	RET

// Shuffle four packed RGB pixels into (R,B) and (G,0) byte pairs. PMADDUBSW
// can then evaluate 77R+29B and 75G without signed-byte coefficient overflow.
DATA rgb_to_rb_shuffle<>+0(SB)/8, $0x0b09080605030200
DATA rgb_to_rb_shuffle<>+8(SB)/8, $0x8080808080808080
GLOBL rgb_to_rb_shuffle<>(SB), RODATA, $16
DATA rgb_to_g_shuffle<>+0(SB)/8, $0x800a800780048001
DATA rgb_to_g_shuffle<>+8(SB)/8, $0x8080808080808080
GLOBL rgb_to_g_shuffle<>(SB), RODATA, $16
DATA rgb_to_rb_weights<>+0(SB)/8, $0x1d4d1d4d1d4d1d4d
DATA rgb_to_rb_weights<>+8(SB)/8, $0x1d4d1d4d1d4d1d4d
GLOBL rgb_to_rb_weights<>(SB), RODATA, $16
DATA rgb_to_g_weights<>+0(SB)/8, $0x004b004b004b004b
DATA rgb_to_g_weights<>+8(SB)/8, $0x004b004b004b004b
GLOBL rgb_to_g_weights<>(SB), RODATA, $16

// func rgbLuma8p8SSSE3(current, output unsafe.Pointer, pixels uintptr)
// Four RGB triples become four exact 77R+150G+29B uint16 values per iteration.
// The caller supplies readable padding after the row's final pixel.
TEXT ·rgbLuma8p8SSSE3(SB), NOSPLIT, $0-24
	MOVQ current+0(FP), DI
	MOVQ output+8(FP), SI
	MOVQ pixels+16(FP), CX

luma_vector:
	CMPQ CX, $4
	JB luma_tail
	MOVOU (DI), X0
	MOVO X0, X1
	PSHUFB rgb_to_rb_shuffle<>(SB), X0
	PSHUFB rgb_to_g_shuffle<>(SB), X1
	PMADDUBSW rgb_to_rb_weights<>(SB), X0
	PMADDUBSW rgb_to_g_weights<>(SB), X1
	PADDW X1, X1
	PADDW X1, X0
	MOVQ X0, (SI)
	ADDQ $12, DI
	ADDQ $8, SI
	SUBQ $4, CX
	JMP luma_vector

	// The final zero to three pixels use scalar multiplies. This tail is at most
	// nine input bytes per row and does not justify masked SIMD machinery.
luma_tail:
	TESTQ CX, CX
	JZ luma_done
luma_tail_loop:
	MOVBLZX (DI), AX
	IMULL $77, AX
	MOVBLZX 1(DI), BX
	IMULL $150, BX
	ADDL BX, AX
	MOVBLZX 2(DI), BX
	IMULL $29, BX
	ADDL BX, AX
	MOVW AX, (SI)
	ADDQ $3, DI
	ADDQ $2, SI
	DECQ CX
	JNZ luma_tail_loop
luma_done:
	RET

// func minMaxUint16SSE41(values unsafe.Pointer, length uintptr) uint32
// Eight lumas are reduced per iteration. The return packs min in the low word
// and max in the high word so no output pointers escape to assembly.
TEXT ·minMaxUint16SSE41(SB), NOSPLIT, $0-20
	MOVQ values+0(FP), SI
	MOVQ length+8(FP), CX
	PCMPEQW X0, X0            // Minimum starts at 0xffff in every lane.
	PXOR X1, X1               // Maximum starts at zero.

minmax_vector:
	CMPQ CX, $8
	JB minmax_reduce
	MOVOU (SI), X2
	PMINUW X2, X0
	PMAXUW X2, X1
	ADDQ $16, SI
	SUBQ $8, CX
	JMP minmax_vector

	// Fold eight lanes to lane zero before processing the scalar remainder.
minmax_reduce:
	MOVO X0, X2
	PSRLDQ $8, X2
	PMINUW X2, X0
	MOVO X0, X2
	PSRLDQ $4, X2
	PMINUW X2, X0
	MOVO X0, X2
	PSRLDQ $2, X2
	PMINUW X2, X0
	PEXTRW $0, X0, AX

	MOVO X1, X2
	PSRLDQ $8, X2
	PMAXUW X2, X1
	MOVO X1, X2
	PSRLDQ $4, X2
	PMAXUW X2, X1
	MOVO X1, X2
	PSRLDQ $2, X2
	PMAXUW X2, X1
	PEXTRW $0, X1, BX

minmax_tail:
	TESTQ CX, CX
	JZ minmax_done
	MOVWLZX (SI), DX
	CMPL DX, AX
	JAE minmax_not_min
	MOVL DX, AX
minmax_not_min:
	CMPL DX, BX
	JBE minmax_not_max
	MOVL DX, BX
minmax_not_max:
	ADDQ $2, SI
	DECQ CX
	JMP minmax_tail

minmax_done:
	SHLL $16, BX
	ORL BX, AX
	MOVL AX, ret+16(FP)
	RET

// func luma8p8ToGraySSE2(values, output unsafe.Pointer, length uintptr)
// Eight 8.8 lumas are rounded, shifted and packed to bytes per iteration.
TEXT ·luma8p8ToGraySSE2(SB), NOSPLIT, $0-24
	MOVQ values+0(FP), SI
	MOVQ output+8(FP), DI
	MOVQ length+16(FP), CX
	MOVQ $0x0080008000800080, AX
	MOVQ AX, X2
	PUNPCKLQDQ X2, X2
	PXOR X3, X3

gray_vector:
	CMPQ CX, $8
	JB gray_tail
	MOVOU (SI), X0
	PADDW X2, X0
	PSRLW $8, X0
	PACKUSWB X3, X0
	MOVQ X0, (DI)
	ADDQ $16, SI
	ADDQ $8, DI
	SUBQ $8, CX
	JMP gray_vector

gray_tail:
	TESTQ CX, CX
	JZ gray_done
gray_tail_loop:
	MOVWLZX (SI), AX
	ADDL $128, AX
	SHRL $8, AX
	MOVB AX, (DI)
	ADDQ $2, SI
	INCQ DI
	DECQ CX
	JNZ gray_tail_loop
gray_done:
	RET

// func horizontalLumaFull2xSSE2(values, output unsafe.Pointer, width uintptr)
// Eight adjacent source pairs become sixteen 4x-weighted horizontal samples.
TEXT ·horizontalLumaFull2xSSE2(SB), NOSPLIT, $0-24
	MOVQ values+0(FP), SI
	MOVQ output+8(FP), DI
	MOVQ width+16(FP), CX
	MOVQ $0x0080008000800080, AX
	MOVQ AX, X7
	PUNPCKLQDQ X7, X7

	// Clamped edge samples are exactly four times rounded source gray.
	MOVWLZX (SI), AX
	ADDL $128, AX
	SHRL $8, AX
	SHLL $2, AX
	MOVW AX, (DI)
	MOVWLZX -2(SI)(CX*2), AX
	ADDL $128, AX
	SHRL $8, AX
	SHLL $2, AX
	MOVW AX, -2(DI)(CX*4)

	// R8 counts source pairs, while SI/DI advance through their first element.
	MOVQ CX, R8
	DECQ R8
	ADDQ $2, DI

horizontal_vector:
	CMPQ R8, $8
	JB horizontal_tail
	MOVOU (SI), X0
	MOVOU 2(SI), X1
	PADDW X7, X0
	PADDW X7, X1
	PSRLW $8, X0
	PSRLW $8, X1

	MOVO X0, X2
	PADDW X0, X2
	PADDW X0, X2
	PADDW X1, X2             // X2 = 3A+B.
	MOVO X1, X3
	PADDW X1, X3
	PADDW X1, X3
	PADDW X0, X3             // X3 = A+3B.
	MOVO X2, X4
	PUNPCKLWL X3, X4
	PUNPCKHWL X3, X2
	MOVOU X4, (DI)
	MOVOU X2, 16(DI)
	ADDQ $16, SI
	ADDQ $32, DI
	SUBQ $8, R8
	JMP horizontal_vector

horizontal_tail:
	TESTQ R8, R8
	JZ horizontal_done
horizontal_tail_loop:
	MOVWLZX (SI), AX
	ADDL $128, AX
	SHRL $8, AX
	MOVWLZX 2(SI), BX
	ADDL $128, BX
	SHRL $8, BX
	LEAL (AX)(AX*2), DX
	ADDL BX, DX
	MOVW DX, (DI)
	LEAL (BX)(BX*2), DX
	ADDL AX, DX
	MOVW DX, 2(DI)
	ADDQ $2, SI
	ADDQ $4, DI
	DECQ R8
	JNZ horizontal_tail_loop
horizontal_done:
	RET

// func verticalRows2xSSE2(current, next, upper, lower unsafe.Pointer, length uintptr)
// Eight columns produce both vertical rows from one pair of intermediate loads.
TEXT ·verticalRows2xSSE2(SB), NOSPLIT, $0-40
	MOVQ current+0(FP), SI
	MOVQ next+8(FP), DI
	MOVQ upper+16(FP), R8
	MOVQ lower+24(FP), R9
	MOVQ length+32(FP), CX
	MOVQ $0x0008000800080008, AX
	MOVQ AX, X7
	PUNPCKLQDQ X7, X7
	PXOR X6, X6

vertical_vector:
	CMPQ CX, $8
	JB vertical_tail
	MOVOU (SI), X0
	MOVOU (DI), X1
	MOVO X0, X2
	PADDW X0, X2
	PADDW X0, X2
	PADDW X1, X2
	PADDW X7, X2
	PSRLW $4, X2
	PACKUSWB X6, X2
	MOVQ X2, (R8)
	MOVO X1, X3
	PADDW X1, X3
	PADDW X1, X3
	PADDW X0, X3
	PADDW X7, X3
	PSRLW $4, X3
	PACKUSWB X6, X3
	MOVQ X3, (R9)
	ADDQ $16, SI
	ADDQ $16, DI
	ADDQ $8, R8
	ADDQ $8, R9
	SUBQ $8, CX
	JMP vertical_vector

vertical_tail:
	TESTQ CX, CX
	JZ vertical_done
vertical_tail_loop:
	MOVWLZX (SI), AX
	MOVWLZX (DI), BX
	LEAL 8(BX)(AX*2), DX
	ADDL AX, DX
	SHRL $4, DX
	MOVB DX, (R8)
	LEAL 8(AX)(BX*2), DX
	ADDL BX, DX
	SHRL $4, DX
	MOVB DX, (R9)
	ADDQ $2, SI
	ADDQ $2, DI
	INCQ R8
	INCQ R9
	DECQ CX
	JNZ vertical_tail_loop
vertical_done:
	RET
