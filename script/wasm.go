// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package script

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math"
)

const (
	WasmPageSize         = 65536
	MaxWasmPages         = 2560 // 160 MiB
	MaxWasmSteps         = 5000000
	MaxWasmCallDepth     = 640
	MaxWasmStack         = 4096
	MaxWasmSectionItems  = 16384
	MaxWasmLocalsPerFunc = 4096
)

var (
	WasmMagic   = [4]byte{0x00, 'a', 's', 'm'}
	WasmVersion = [4]byte{0x01, 0x00, 0x00, 0x00}
)

type WasmValType uint8

const (
	ValI32 WasmValType = 0x7F
	ValI64 WasmValType = 0x7E
	ValF32 WasmValType = 0x7D
	ValF64 WasmValType = 0x7C
)

type WasmValue struct {
	Type WasmValType
	Bits uint64
}

func I32Val(v int32) WasmValue   { return WasmValue{Type: ValI32, Bits: uint64(uint32(v))} }
func I64Val(v int64) WasmValue   { return WasmValue{Type: ValI64, Bits: uint64(v)} }
func F32Val(v float32) WasmValue { return WasmValue{Type: ValF32, Bits: uint64(math.Float32bits(v))} }
func F64Val(v float64) WasmValue { return WasmValue{Type: ValF64, Bits: math.Float64bits(v)} }

func (v WasmValue) AsI32() int32   { return int32(uint32(v.Bits)) }
func (v WasmValue) AsI64() int64   { return int64(v.Bits) }
func (v WasmValue) AsF32() float32 { return math.Float32frombits(uint32(v.Bits)) }
func (v WasmValue) AsF64() float64 { return math.Float64frombits(v.Bits) }

type WasmFuncType struct {
	Params  []WasmValType
	Results []WasmValType
}

type WasmExportKind uint8

const (
	ExportFunc   WasmExportKind = 0x00
	ExportTable  WasmExportKind = 0x01
	ExportMemory WasmExportKind = 0x02
	ExportGlobal WasmExportKind = 0x03
)

type WasmExport struct {
	Name  string
	Kind  WasmExportKind
	Index uint32
}

type WasmCodeBody struct {
	Locals []WasmValType
	Code   []byte
}

type WasmGlobalDef struct {
	Type    WasmValType
	Mutable bool
	Init    WasmValue
}

type WasmDataSegment struct {
	Offset uint32
	Bytes  []byte
}

type WasmModule struct {
	Types        []WasmFuncType
	Functions    []uint32
	MemoryMin    uint32
	MemoryMax    uint32
	HasMemory    bool
	Globals      []WasmGlobalDef
	Exports      []WasmExport
	Codes        []WasmCodeBody
	DataSegments []WasmDataSegment
}

func ValidateWasm(raw []byte) bool {
	_, err := DecodeWasmModule(raw)
	return err == nil
}

func DecodeWasmModule(raw []byte) (*WasmModule, error) {
	if len(raw) < 8 {
		return nil, fmt.Errorf("wasm binary too short")
	}
	if !bytes.Equal(raw[:4], WasmMagic[:]) || !bytes.Equal(raw[4:8], WasmVersion[:]) {
		return nil, fmt.Errorf("invalid wasm magic or version")
	}
	mod := &WasmModule{}
	pos := 8
	lastSec := uint8(0)
	for pos < len(raw) {
		secID := raw[pos]
		pos++
		secLen, n, err := readUleb128(raw[pos:])
		if err != nil || int(secLen) < 0 || int(secLen) > len(raw)-pos-n {
			return nil, fmt.Errorf("truncated section %d", secID)
		}
		pos += n
		payload := raw[pos : pos+int(secLen)]
		pos += int(secLen)
		if secID != 0 {
			if secID <= lastSec {
				return nil, fmt.Errorf("out-of-order section %d", secID)
			}
			lastSec = secID
		}
		if err := parseWasmSection(mod, secID, payload); err != nil {
			return nil, err
		}
	}
	if len(mod.Functions) != len(mod.Codes) {
		return nil, fmt.Errorf("function count (%d) != code count (%d)", len(mod.Functions), len(mod.Codes))
	}
	for _, typeIdx := range mod.Functions {
		if int(typeIdx) >= len(mod.Types) {
			return nil, fmt.Errorf("function type index %d out of range", typeIdx)
		}
	}
	for _, exp := range mod.Exports {
		if exp.Kind == ExportFunc && int(exp.Index) >= len(mod.Functions) {
			return nil, fmt.Errorf("exported function index %d out of range", exp.Index)
		}
	}
	return mod, nil
}

func parseWasmSection(mod *WasmModule, id uint8, p []byte) error {
	r := 0
	switch id {
	case 0: // custom
		return nil
	case 1: // type section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid type section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			if r >= len(p) || p[r] != 0x60 {
				return fmt.Errorf("expected functype 0x60")
			}
			r++
			pCnt, n, err := readUleb128(p[r:])
			if err != nil || pCnt > 256 || int(pCnt) > len(p)-r-n {
				return fmt.Errorf("invalid param count")
			}
			r += n
			params := make([]WasmValType, pCnt)
			for k := uint32(0); k < pCnt; k++ {
				params[k] = WasmValType(p[r+int(k)])
			}
			r += int(pCnt)
			rCnt, n, err := readUleb128(p[r:])
			if err != nil || rCnt > 256 || int(rCnt) > len(p)-r-n {
				return fmt.Errorf("invalid result count")
			}
			r += n
			results := make([]WasmValType, rCnt)
			for k := uint32(0); k < rCnt; k++ {
				results[k] = WasmValType(p[r+int(k)])
			}
			r += int(rCnt)
			mod.Types = append(mod.Types, WasmFuncType{Params: params, Results: results})
		}
	case 3: // function section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid function section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			idx, n, err := readUleb128(p[r:])
			if err != nil {
				return err
			}
			r += n
			mod.Functions = append(mod.Functions, idx)
		}
	case 5: // memory section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt == 0 || cnt > 1 {
			return fmt.Errorf("invalid memory section count")
		}
		r += n
		if r >= len(p) {
			return fmt.Errorf("truncated memory section")
		}
		flags := p[r]
		r++
		minP, n, err := readUleb128(p[r:])
		if err != nil {
			return err
		}
		r += n
		maxP := uint32(MaxWasmPages)
		if flags&1 != 0 {
			m, _, err := readUleb128(p[r:])
			if err != nil {
				return err
			}
			if m < maxP {
				maxP = m
			}
		}
		if minP > MaxWasmPages {
			return fmt.Errorf("memory min pages exceeds cap")
		}
		mod.HasMemory = true
		mod.MemoryMin = minP
		mod.MemoryMax = maxP
	case 6: // global section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid global section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			if r+2 > len(p) {
				return fmt.Errorf("truncated global")
			}
			vt := WasmValType(p[r])
			mut := p[r+1] != 0
			r += 2
			initVal, adv, err := evalConstExpr(p[r:], vt)
			if err != nil {
				return err
			}
			r += adv
			mod.Globals = append(mod.Globals, WasmGlobalDef{Type: vt, Mutable: mut, Init: initVal})
		}
	case 7: // export section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid export section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			nLen, n, err := readUleb128(p[r:])
			if err != nil || int(nLen) < 0 || int(nLen)+1 > len(p)-r-n {
				return fmt.Errorf("truncated export")
			}
			r += n
			name := string(p[r : r+int(nLen)])
			r += int(nLen)
			kind := WasmExportKind(p[r])
			r++
			idx, n, err := readUleb128(p[r:])
			if err != nil {
				return err
			}
			r += n
			mod.Exports = append(mod.Exports, WasmExport{Name: name, Kind: kind, Index: idx})
		}
	case 10: // code section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid code section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			bSize, n, err := readUleb128(p[r:])
			if err != nil || int(bSize) < 0 || int(bSize) > len(p)-r-n {
				return fmt.Errorf("truncated code body")
			}
			r += n
			bodyBytes := p[r : r+int(bSize)]
			r += int(bSize)
			br := 0
			lVecCnt, n, err := readUleb128(bodyBytes[br:])
			if err != nil || lVecCnt > MaxWasmLocalsPerFunc {
				return fmt.Errorf("invalid local vector count")
			}
			br += n
			var locals []WasmValType
			for k := uint32(0); k < lVecCnt; k++ {
				rep, n, err := readUleb128(bodyBytes[br:])
				if err != nil || br+n >= len(bodyBytes) || rep > MaxWasmLocalsPerFunc || len(locals)+int(rep) > MaxWasmLocalsPerFunc {
					return fmt.Errorf("invalid local decl")
				}
				br += n
				lt := WasmValType(bodyBytes[br])
				br++
				for c := uint32(0); c < rep; c++ {
					locals = append(locals, lt)
				}
			}
			mod.Codes = append(mod.Codes, WasmCodeBody{Locals: locals, Code: bodyBytes[br:]})
		}
	case 11: // data section
		cnt, n, err := readUleb128(p[r:])
		if err != nil || cnt > MaxWasmSectionItems {
			return fmt.Errorf("invalid data section count")
		}
		r += n
		for i := uint32(0); i < cnt; i++ {
			_, n, err := readUleb128(p[r:])
			if err != nil {
				return err
			}
			r += n
			offVal, adv, err := evalConstExpr(p[r:], ValI32)
			if err != nil {
				return err
			}
			r += adv
			dLen, n, err := readUleb128(p[r:])
			if err != nil || int(dLen) < 0 || int(dLen) > len(p)-r-n {
				return fmt.Errorf("truncated data segment")
			}
			r += n
			segBytes := make([]byte, dLen)
			copy(segBytes, p[r:r+int(dLen)])
			r += int(dLen)
			mod.DataSegments = append(mod.DataSegments, WasmDataSegment{
				Offset: uint32(offVal.AsI32()),
				Bytes:  segBytes,
			})
		}
	}
	return nil
}

func evalConstExpr(b []byte, vt WasmValType) (WasmValue, int, error) {
	if len(b) < 2 {
		return WasmValue{}, 0, fmt.Errorf("const expr too short")
	}
	op := b[0]
	r := 1
	var val WasmValue
	switch op {
	case 0x41: // i32.const
		v, n, err := readSleb128(b[r:])
		if err != nil {
			return WasmValue{}, 0, err
		}
		r += n
		val = I32Val(int32(v))
	case 0x42: // i64.const
		v, n, err := readSleb128(b[r:])
		if err != nil {
			return WasmValue{}, 0, err
		}
		r += n
		val = I64Val(v)
	default:
		return WasmValue{}, 0, fmt.Errorf("unsupported const expr opcode 0x%02x", op)
	}
	if r >= len(b) || b[r] != 0x0B {
		return WasmValue{}, 0, fmt.Errorf("missing 0x0B end in const expr")
	}
	_ = vt
	return val, r + 1, nil
}

type WasmMemory struct {
	Data     []byte
	MaxPages uint32
}

func NewWasmMemory(initialPages, maxPages uint32) (*WasmMemory, error) {
	if maxPages == 0 || maxPages > MaxWasmPages {
		maxPages = MaxWasmPages
	}
	if initialPages > maxPages {
		return nil, fmt.Errorf("initial pages (%d) > max pages (%d)", initialPages, maxPages)
	}
	return &WasmMemory{
		Data:     make([]byte, int(initialPages)*WasmPageSize),
		MaxPages: maxPages,
	}, nil
}

func (m *WasmMemory) Pages() uint32 {
	return uint32(len(m.Data) / WasmPageSize)
}

func (m *WasmMemory) Grow(delta uint32) int32 {
	old := m.Pages()
	next := old + delta
	if next > m.MaxPages || next > MaxWasmPages {
		return -1
	}
	if delta > 0 {
		m.Data = append(m.Data, make([]byte, int(delta)*WasmPageSize)...)
	}
	return int32(old)
}

type WasmInstance struct {
	Module  *WasmModule
	Memory  *WasmMemory
	Globals []WasmValue
}

func InstantiateWasm(mod *WasmModule) (*WasmInstance, error) {
	var mem *WasmMemory
	if mod.HasMemory {
		var err error
		mem, err = NewWasmMemory(mod.MemoryMin, mod.MemoryMax)
		if err != nil {
			return nil, err
		}
	} else {
		mem, _ = NewWasmMemory(1, MaxWasmPages)
	}
	globals := make([]WasmValue, len(mod.Globals))
	for i, g := range mod.Globals {
		globals[i] = g.Init
	}
	for _, seg := range mod.DataSegments {
		end := int64(seg.Offset) + int64(len(seg.Bytes))
		if end > int64(len(mem.Data)) {
			return nil, fmt.Errorf("data segment out of bounds")
		}
		copy(mem.Data[int(seg.Offset):int(end)], seg.Bytes)
	}
	return &WasmInstance{
		Module:  mod,
		Memory:  mem,
		Globals: globals,
	}, nil
}

func (inst *WasmInstance) CallExport(name string, args ...WasmValue) ([]WasmValue, error) {
	for _, exp := range inst.Module.Exports {
		if exp.Name == name && exp.Kind == ExportFunc {
			steps := 0
			return inst.execFunc(exp.Index, args, 0, &steps)
		}
	}
	return nil, fmt.Errorf("exported function %q not found", name)
}

func (inst *WasmInstance) execFunc(funcIdx uint32, args []WasmValue, depth int, steps *int) ([]WasmValue, error) {
	if depth > MaxWasmCallDepth {
		return nil, fmt.Errorf("call stack depth exceeded")
	}
	if int(funcIdx) >= len(inst.Module.Functions) || int(funcIdx) >= len(inst.Module.Codes) {
		return nil, fmt.Errorf("function index out of range")
	}
	typeIdx := inst.Module.Functions[funcIdx]
	if int(typeIdx) >= len(inst.Module.Types) {
		return nil, fmt.Errorf("type index out of range")
	}
	fnType := inst.Module.Types[typeIdx]
	body := inst.Module.Codes[funcIdx]

	locals := make([]WasmValue, len(fnType.Params)+len(body.Locals))
	copy(locals, args)
	for i, lt := range body.Locals {
		locals[len(fnType.Params)+i] = WasmValue{Type: lt}
	}

	var stack []WasmValue
	pop := func() (WasmValue, error) {
		if len(stack) == 0 {
			return WasmValue{}, fmt.Errorf("wasm stack underflow")
		}
		v := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		return v, nil
	}
	push := func(v WasmValue) error {
		if len(stack) >= MaxWasmStack {
			return fmt.Errorf("wasm operand stack overflow")
		}
		stack = append(stack, v)
		return nil
	}

	code := body.Code
	pc := 0
	for pc < len(code) {
		*steps++
		if *steps > MaxWasmSteps {
			return nil, fmt.Errorf("wasm instruction budget exceeded")
		}
		op := code[pc]
		pc++
		switch op {
		case 0x01: // nop
		case 0x0B: // end
			if pc >= len(code) {
				break
			}
		case 0x0F: // return
			pc = len(code)
		case 0x10: // call
			callee, n, err := readUleb128(code[pc:])
			if err != nil {
				return nil, err
			}
			pc += n
			if int(callee) >= len(inst.Module.Functions) {
				return nil, fmt.Errorf("call target %d out of range", callee)
			}
			calleeTypeIdx := inst.Module.Functions[callee]
			if int(calleeTypeIdx) >= len(inst.Module.Types) {
				return nil, fmt.Errorf("call type index %d out of range", calleeTypeIdx)
			}
			calleeType := inst.Module.Types[calleeTypeIdx]
			callArgs := make([]WasmValue, len(calleeType.Params))
			for i := len(calleeType.Params) - 1; i >= 0; i-- {
				v, err := pop()
				if err != nil {
					return nil, err
				}
				callArgs[i] = v
			}
			res, err := inst.execFunc(callee, callArgs, depth+1, steps)
			if err != nil {
				return nil, err
			}
			for _, rv := range res {
				if err := push(rv); err != nil {
					return nil, err
				}
			}
		case 0x1A: // drop
			if _, err := pop(); err != nil {
				return nil, err
			}
		case 0x20: // local.get
			idx, n, err := readUleb128(code[pc:])
			if err != nil || int(idx) >= len(locals) {
				return nil, fmt.Errorf("invalid local.get")
			}
			pc += n
			if err := push(locals[idx]); err != nil {
				return nil, err
			}
		case 0x21: // local.set
			idx, n, err := readUleb128(code[pc:])
			if err != nil || int(idx) >= len(locals) {
				return nil, fmt.Errorf("invalid local.set")
			}
			pc += n
			v, err := pop()
			if err != nil {
				return nil, err
			}
			locals[idx] = v
		case 0x22: // local.tee
			idx, n, err := readUleb128(code[pc:])
			if err != nil || int(idx) >= len(locals) || len(stack) == 0 {
				return nil, fmt.Errorf("invalid local.tee")
			}
			pc += n
			locals[idx] = stack[len(stack)-1]
		case 0x23: // global.get
			idx, n, err := readUleb128(code[pc:])
			if err != nil || int(idx) >= len(inst.Globals) {
				return nil, fmt.Errorf("invalid global.get")
			}
			pc += n
			if err := push(inst.Globals[idx]); err != nil {
				return nil, err
			}
		case 0x24: // global.set
			idx, n, err := readUleb128(code[pc:])
			if err != nil || int(idx) >= len(inst.Globals) {
				return nil, fmt.Errorf("invalid global.set")
			}
			pc += n
			v, err := pop()
			if err != nil {
				return nil, err
			}
			inst.Globals[idx] = v
		case 0x28: // i32.load
			_, n1, err1 := readUleb128(code[pc:])
			if err1 != nil {
				return nil, err1
			}
			off, n2, err2 := readUleb128(code[pc+n1:])
			if err2 != nil {
				return nil, err2
			}
			pc += n1 + n2
			addrV, err := pop()
			if err != nil {
				return nil, err
			}
			ea := uint64(uint32(addrV.AsI32())) + uint64(off)
			if ea+4 > uint64(len(inst.Memory.Data)) {
				return nil, fmt.Errorf("i32.load out of bounds")
			}
			val := int32(binary.LittleEndian.Uint32(inst.Memory.Data[int(ea) : int(ea)+4]))
			if err := push(I32Val(val)); err != nil {
				return nil, err
			}
		case 0x36: // i32.store
			_, n1, err1 := readUleb128(code[pc:])
			if err1 != nil {
				return nil, err1
			}
			off, n2, err2 := readUleb128(code[pc+n1:])
			if err2 != nil {
				return nil, err2
			}
			pc += n1 + n2
			valV, err1 := pop()
			addrV, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.store stack underflow")
			}
			ea := uint64(uint32(addrV.AsI32())) + uint64(off)
			if ea+4 > uint64(len(inst.Memory.Data)) {
				return nil, fmt.Errorf("i32.store out of bounds")
			}
			binary.LittleEndian.PutUint32(inst.Memory.Data[int(ea):int(ea)+4], uint32(valV.AsI32()))
		case 0x3F: // memory.size
			if pc >= len(code) {
				return nil, fmt.Errorf("truncated memory.size")
			}
			pc++
			if err := push(I32Val(int32(inst.Memory.Pages()))); err != nil {
				return nil, err
			}
		case 0x40: // memory.grow
			if pc >= len(code) {
				return nil, fmt.Errorf("truncated memory.grow")
			}
			pc++
			delta, err := pop()
			if err != nil {
				return nil, err
			}
			if err := push(I32Val(inst.Memory.Grow(uint32(delta.AsI32())))); err != nil {
				return nil, err
			}
		case 0x41: // i32.const
			v, n, err := readSleb128(code[pc:])
			if err != nil {
				return nil, err
			}
			pc += n
			if err := push(I32Val(int32(v))); err != nil {
				return nil, err
			}
		case 0x42: // i64.const
			v, n, err := readSleb128(code[pc:])
			if err != nil {
				return nil, err
			}
			pc += n
			if err := push(I64Val(v)); err != nil {
				return nil, err
			}
		case 0x6A: // i32.add
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.add stack underflow")
			}
			if err := push(I32Val(a.AsI32() + b.AsI32())); err != nil {
				return nil, err
			}
		case 0x6B: // i32.sub
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.sub stack underflow")
			}
			if err := push(I32Val(a.AsI32() - b.AsI32())); err != nil {
				return nil, err
			}
		case 0x6C: // i32.mul
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.mul stack underflow")
			}
			if err := push(I32Val(a.AsI32() * b.AsI32())); err != nil {
				return nil, err
			}
		case 0x71: // i32.and
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.and stack underflow")
			}
			if err := push(I32Val(a.AsI32() & b.AsI32())); err != nil {
				return nil, err
			}
		case 0x72: // i32.or
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.or stack underflow")
			}
			if err := push(I32Val(a.AsI32() | b.AsI32())); err != nil {
				return nil, err
			}
		case 0x73: // i32.xor
			b, err1 := pop()
			a, err2 := pop()
			if err1 != nil || err2 != nil {
				return nil, fmt.Errorf("i32.xor stack underflow")
			}
			if err := push(I32Val(a.AsI32() ^ b.AsI32())); err != nil {
				return nil, err
			}
		default:
			return nil, fmt.Errorf("unsupported wasm opcode 0x%02x", op)
		}
	}

	resCnt := len(fnType.Results)
	if len(stack) < resCnt {
		return nil, fmt.Errorf("missing return values on wasm stack")
	}
	return stack[len(stack)-resCnt:], nil
}

func readUleb128(b []byte) (uint32, int, error) {
	var res uint32
	var shift uint
	for i := 0; i < len(b) && i < 5; i++ {
		byteVal := b[i]
		res |= uint32(byteVal&0x7F) << shift
		if byteVal&0x80 == 0 {
			return res, i + 1, nil
		}
		shift += 7
	}
	return 0, 0, fmt.Errorf("invalid uleb128")
}

func readSleb128(b []byte) (int64, int, error) {
	var res int64
	var shift uint
	for i := 0; i < len(b) && i < 10; i++ {
		byteVal := b[i]
		res |= int64(byteVal&0x7F) << shift
		shift += 7
		if byteVal&0x80 == 0 {
			if shift < 64 && (byteVal&0x40) != 0 {
				res |= -(1 << shift)
			}
			return res, i + 1, nil
		}
	}
	return 0, 0, fmt.Errorf("invalid sleb128")
}
