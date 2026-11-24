/*
 * Copyright (C) 2013 The Android Open Source Project
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *  * Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 *  * Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in
 *    the documentation and/or other materials provided with the
 *    distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
 * "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
 * LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
 * FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
 * COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
 * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
 * BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS
 * OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED
 * AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
 * OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */

// Experimental transparent ARM64 entry. Keep successful syscalls frameless:
// clone and signal-return paths cannot tolerate an ordinary C wrapper frame.
#if !defined(__aarch64__)
#error "This experiment only supports AArch64"
#endif
.text
.p2align 2
.global syscall
#ifdef FRONTCAP_WAIT_APP
// Isolated control DSO needs startup TLS allocation without interposing on ART.
.hidden syscall
#endif
.type syscall,%function
syscall:
.cfi_startproc
#ifdef FRONTCAP_VIRTUAL_FUTEX
    cmp x0,#98
    b.eq fc_virtual_futex
#endif
#ifdef FRONTCAP_TRACE_FUTEX
    cmp x0,#98
    b.ne 2f
    stp x0,x1,[sp,#-80]!
.cfi_def_cfa_offset 80
    stp x2,x3,[sp,#16]
    stp x4,x5,[sp,#32]
    stp x6,x7,[sp,#48]
    str x30,[sp,#64]
.cfi_offset x30,-16
    mov x7,x30
    bl fc_trace_futex
    ldr x30,[sp,#64]
    ldp x6,x7,[sp,#48]
    ldp x4,x5,[sp,#32]
    ldp x2,x3,[sp,#16]
    ldp x0,x1,[sp],#80
.cfi_restore x30
.cfi_def_cfa_offset 0
2:
#endif

.global fc_raw_syscall
.hidden fc_raw_syscall
fc_raw_syscall:
    mov x8,x0
    mov x0,x1
    mov x1,x2
    mov x2,x3
    mov x3,x4
    mov x4,x5
    mov x5,x6
    svc #0
    cmn x0,#4096
    b.hi 1f
    ret
1:
    stp x19,x30,[sp,#-16]!
.cfi_def_cfa_offset 16
.cfi_offset x19,-16
.cfi_offset x30,-8
    neg w19,w0
    bl __errno
    str w19,[x0]
    mov x0,#-1
    ldp x19,x30,[sp],#16
.cfi_restore x19
.cfi_restore x30
.cfi_def_cfa_offset 0
    ret
.cfi_endproc
.size syscall,.-syscall
.section .note.GNU-stack,"",%progbits
