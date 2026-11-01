// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato. Verify canonical repository history at https://github.com/buntatoes

package dom

import (
	"fmt"
	"strconv"
	"strings"
)

const (
	RenderingEngineName = "Forge"
	JsEngineName        = "Cinder"
	CssEngineName       = "Prism"

	MaxDomDepth        = 256
	MaxDomNodes        = 50000
	MaxAttrsPerElement = 128
)

type NodeId uint32

func NodeIdFromRaw(index uint32) NodeId {
	return NodeId(index)
}

func (id NodeId) Index() int {
	return int(id)
}

type TreeError int

const (
	TreeErrorNone TreeError = iota
	TreeErrorMissing
	TreeErrorCycle
)

func (e TreeError) Error() string {
	switch e {
	case TreeErrorMissing:
		return "missing node in document"
	case TreeErrorCycle:
		return "cycle detected in document tree"
	default:
		return "no error"
	}
}

type StyleSheetSourceKind int

const (
	StyleSheetLink StyleSheetSourceKind = iota
	StyleSheetInline
)

type StyleSheetSource struct {
	Kind      StyleSheetSourceKind
	Href      string
	Integrity string
	CSS       string
	Media     string
}

type Attr struct {
	Name  string
	Value string
}

type NodeKind int

const (
	NodeKindDocument NodeKind = iota
	NodeKindElement
	NodeKindText
)

type NodeData struct {
	Kind  NodeKind
	Tag   string
	Attrs []Attr
	Text  string
}

func NewElementData(tag string) NodeData {
	return NodeData{
		Kind: NodeKindElement,
		Tag:  strings.ToLower(strings.TrimSpace(tag)),
	}
}

func NewTextData(text string) NodeData {
	return NodeData{
		Kind: NodeKindText,
		Text: text,
	}
}

func (d *NodeData) IsElement() bool {
	return d.Kind == NodeKindElement
}

func (d *NodeData) IsText() bool {
	return d.Kind == NodeKindText
}

func (d *NodeData) Attr(name string) (string, bool) {
	if d.Kind != NodeKindElement {
		return "", false
	}
	for _, a := range d.Attrs {
		if strings.EqualFold(a.Name, name) {
			return a.Value, true
		}
	}
	return "", false
}

func (d *NodeData) ID() string {
	v, _ := d.Attr("id")
	return v
}

func (d *NodeData) HasClass(className string) bool {
	cls, ok := d.Attr("class")
	if !ok || className == "" {
		return false
	}
	for _, tok := range strings.Fields(cls) {
		if tok == className {
			return true
		}
	}
	return false
}

func (d *NodeData) SetAttr(name, value string) {
	if d.Kind != NodeKindElement {
		return
	}
	key := strings.ToLower(strings.TrimSpace(name))
	if key == "" {
		return
	}
	for i := range d.Attrs {
		if d.Attrs[i].Name == key {
			d.Attrs[i].Value = value
			return
		}
	}
	if len(d.Attrs) < MaxAttrsPerElement {
		d.Attrs = append(d.Attrs, Attr{Name: key, Value: value})
	}
}

func (d *NodeData) RemoveAttr(name string) bool {
	if d.Kind != NodeKindElement {
		return false
	}
	before := len(d.Attrs)
	kept := d.Attrs[:0]
	for _, a := range d.Attrs {
		if !strings.EqualFold(a.Name, name) {
			kept = append(kept, a)
		}
	}
	d.Attrs = kept
	return len(d.Attrs) != before
}

func (d *NodeData) AddClass(className string) {
	className = strings.TrimSpace(className)
	if className == "" || d.HasClass(className) {
		return
	}
	existing, ok := d.Attr("class")
	existing = strings.TrimSpace(existing)
	if ok && existing != "" {
		d.SetAttr("class", existing+" "+className)
	} else {
		d.SetAttr("class", className)
	}
}

func (d *NodeData) RemoveClass(className string) {
	className = strings.TrimSpace(className)
	existing, ok := d.Attr("class")
	if !ok {
		return
	}
	var kept []string
	for _, tok := range strings.Fields(existing) {
		if tok != className {
			kept = append(kept, tok)
		}
	}
	d.SetAttr("class", strings.Join(kept, " "))
}

func (d *NodeData) ToggleClass(className string) bool {
	if d.HasClass(className) {
		d.RemoveClass(className)
		return false
	}
	d.AddClass(className)
	return true
}

type Node struct {
	ID       NodeId
	Parent   *NodeId
	Children []NodeId
	Data     NodeData
}

type CanvasSurface struct {
	Width  uint32
	Height uint32
	RGBA   []byte
}

type ShadowRootInfo struct {
	Root NodeId
	Open bool
}

type ScrollOffset struct {
	X uint32
	Y uint32
}

type Document struct {
	nodes          []Node
	html           NodeId
	head           NodeId
	body           NodeId
	shadowRoots    map[NodeId]ShadowRootInfo
	shadowHosts    map[NodeId]NodeId
	customElements []string
	canvasSurfaces map[NodeId]*CanvasSurface
	inputCursors   map[NodeId]int
	hoveredNode    *NodeId
	activeNode     *NodeId
	focusedNode    *NodeId
	targetFragment string
	adoptedStyles  []string
	scrollOffsets  map[NodeId]ScrollOffset
	customValidity map[NodeId]string
	dirtyNodes     []NodeId
	generation     uint64
}

func NewDocument() *Document {
	doc := &Document{
		nodes:          make([]Node, 0, 16),
		shadowRoots:    make(map[NodeId]ShadowRootInfo),
		shadowHosts:    make(map[NodeId]NodeId),
		canvasSurfaces: make(map[NodeId]*CanvasSurface),
		inputCursors:   make(map[NodeId]int),
		scrollOffsets:  make(map[NodeId]ScrollOffset),
		customValidity: make(map[NodeId]string),
	}
	root := doc.allocNode(NodeData{Kind: NodeKindDocument})
	html := doc.CreateElement("html")
	head := doc.CreateElement("head")
	body := doc.CreateElement("body")
	_ = doc.Append(root, html)
	_ = doc.Append(html, head)
	_ = doc.Append(html, body)
	doc.html = html
	doc.head = head
	doc.body = body
	doc.dirtyNodes = nil
	doc.generation = 0
	return doc
}

func (doc *Document) allocNode(data NodeData) NodeId {
	id := NodeId(len(doc.nodes))
	doc.nodes = append(doc.nodes, Node{
		ID:   id,
		Data: data,
	})
	return id
}

func (doc *Document) markDirty(id NodeId) {
	doc.generation++
	for _, existing := range doc.dirtyNodes {
		if existing == id {
			return
		}
	}
	doc.dirtyNodes = append(doc.dirtyNodes, id)
}

func (doc *Document) Root() NodeId       { return NodeId(0) }
func (doc *Document) HTML() NodeId       { return doc.html }
func (doc *Document) Head() NodeId       { return doc.head }
func (doc *Document) Body() NodeId       { return doc.body }
func (doc *Document) NodeCount() int     { return len(doc.nodes) }
func (doc *Document) Generation() uint64 { return doc.generation }

func (doc *Document) DirtyStyleNodes() []NodeId {
	out := make([]NodeId, len(doc.dirtyNodes))
	copy(out, doc.dirtyNodes)
	return out
}

func (doc *Document) TakeDirtyStyleNodes() []NodeId {
	out := doc.DirtyStyleNodes()
	doc.dirtyNodes = nil
	return out
}

func (doc *Document) Node(id NodeId) *Node {
	idx := id.Index()
	if idx < 0 || idx >= len(doc.nodes) {
		return nil
	}
	return &doc.nodes[idx]
}

func (doc *Document) Children(id NodeId) []NodeId {
	n := doc.Node(id)
	if n == nil {
		return nil
	}
	return n.Children
}

func (doc *Document) Parent(id NodeId) (NodeId, bool) {
	n := doc.Node(id)
	if n == nil || n.Parent == nil {
		return 0, false
	}
	return *n.Parent, true
}

func (doc *Document) CreateElement(tag string) NodeId {
	return doc.allocNode(NewElementData(tag))
}

func (doc *Document) CreateText(text string) NodeId {
	return doc.allocNode(NewTextData(text))
}

func (doc *Document) Append(parent, child NodeId) error {
	if doc.Node(parent) == nil || doc.Node(child) == nil {
		return TreeErrorMissing
	}
	if parent == child {
		return TreeErrorCycle
	}
	cur := parent
	for {
		p, ok := doc.Parent(cur)
		if !ok {
			break
		}
		if p == child {
			return TreeErrorCycle
		}
		cur = p
	}
	doc.detach(child)
	pCopy := parent
	doc.nodes[child.Index()].Parent = &pCopy
	doc.nodes[parent.Index()].Children = append(doc.nodes[parent.Index()].Children, child)
	doc.markDirty(parent)
	return nil
}

func (doc *Document) InsertBefore(parent, newChild, refChild NodeId) error {
	if doc.Node(parent) == nil || doc.Node(newChild) == nil {
		return TreeErrorMissing
	}
	if newChild == refChild {
		return nil
	}
	if err := doc.Append(parent, newChild); err != nil {
		return err
	}
	children := doc.nodes[parent.Index()].Children
	if len(children) <= 1 {
		return nil
	}
	// Pop newChild from end and insert before refChild
	children = children[:len(children)-1]
	idx := -1
	for i, c := range children {
		if c == refChild {
			idx = i
			break
		}
	}
	if idx < 0 {
		children = append(children, newChild)
	} else {
		updated := make([]NodeId, 0, len(children)+1)
		updated = append(updated, children[:idx]...)
		updated = append(updated, newChild)
		updated = append(updated, children[idx:]...)
		children = updated
	}
	doc.nodes[parent.Index()].Children = children
	return nil
}

func (doc *Document) detach(id NodeId) {
	n := doc.Node(id)
	if n == nil || n.Parent == nil {
		return
	}
	p := *n.Parent
	n.Parent = nil
	if pn := doc.Node(p); pn != nil {
		kept := pn.Children[:0]
		for _, c := range pn.Children {
			if c != id {
				kept = append(kept, c)
			}
		}
		pn.Children = kept
		doc.markDirty(p)
	}
}

func (doc *Document) RemoveNode(id NodeId) {
	doc.detach(id)
}

func (doc *Document) TextContent(id NodeId) string {
	var b strings.Builder
	doc.collectText(id, &b)
	return b.String()
}

func (doc *Document) collectText(id NodeId, b *strings.Builder) {
	n := doc.Node(id)
	if n == nil {
		return
	}
	if n.Data.Kind == NodeKindText {
		b.WriteString(n.Data.Text)
		return
	}
	for _, c := range n.Children {
		doc.collectText(c, b)
	}
}

func (doc *Document) SetTextContent(id NodeId, text string) {
	n := doc.Node(id)
	if n == nil {
		return
	}
	if n.Data.Kind == NodeKindText {
		n.Data.Text = text
		if n.Parent != nil {
			doc.markDirty(*n.Parent)
		}
		return
	}
	for _, c := range append([]NodeId(nil), n.Children...) {
		doc.detach(c)
	}
	if text != "" {
		t := doc.CreateText(text)
		_ = doc.Append(id, t)
	}
	doc.markDirty(id)
}

func (doc *Document) SetInnerHTML(id NodeId, htmlSrc string) {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return
	}
	for _, c := range append([]NodeId(nil), n.Children...) {
		doc.detach(c)
	}
	frag := ParseHTML(htmlSrc)
	for _, c := range frag.Children(frag.Body()) {
		imported := doc.ImportSubtree(frag, c)
		_ = doc.Append(id, imported)
	}
	doc.markDirty(id)
}

func (doc *Document) ImportSubtree(other *Document, root NodeId) NodeId {
	return doc.importSubtreeDepth(other, root, 0)
}

func (doc *Document) importSubtreeDepth(other *Document, root NodeId, depth int) NodeId {
	if other == nil || doc.NodeCount() >= MaxDomNodes || depth > MaxDomDepth {
		return doc.CreateText("")
	}
	src := other.Node(root)
	if src == nil {
		return doc.CreateText("")
	}
	cpData := NodeData{
		Kind: src.Data.Kind,
		Tag:  src.Data.Tag,
		Text: src.Data.Text,
	}
	if len(src.Data.Attrs) > 0 {
		cpData.Attrs = make([]Attr, len(src.Data.Attrs))
		copy(cpData.Attrs, src.Data.Attrs)
	}
	newID := doc.allocNode(cpData)
	for _, c := range src.Children {
		if doc.NodeCount() >= MaxDomNodes {
			break
		}
		childID := doc.importSubtreeDepth(other, c, depth+1)
		_ = doc.Append(newID, childID)
	}
	return newID
}

func (doc *Document) GetElementByID(idAttr string) (NodeId, bool) {
	if idAttr == "" {
		return 0, false
	}
	return doc.findByID(doc.Root(), idAttr)
}

func (doc *Document) findByID(cur NodeId, idAttr string) (NodeId, bool) {
	n := doc.Node(cur)
	if n == nil {
		return 0, false
	}
	if n.Data.Kind == NodeKindElement && n.Data.ID() == idAttr {
		return cur, true
	}
	for _, c := range n.Children {
		if found, ok := doc.findByID(c, idAttr); ok {
			return found, true
		}
	}
	return 0, false
}

func (doc *Document) GetElementsByTagName(tag string) []NodeId {
	target := strings.ToLower(strings.TrimSpace(tag))
	var out []NodeId
	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement && (target == "*" || n.Data.Tag == target) {
			out = append(out, cur)
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())
	return out
}

func (doc *Document) GetElementsByClassName(classNames string) []NodeId {
	tokens := strings.Fields(classNames)
	if len(tokens) == 0 {
		return nil
	}
	var out []NodeId
	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement {
			allMatch := true
			for _, t := range tokens {
				if !n.Data.HasClass(t) {
					allMatch = false
					break
				}
			}
			if allMatch {
				out = append(out, cur)
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())
	return out
}

func (doc *Document) GetElementsByName(name string) []NodeId {
	var out []NodeId
	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement {
			if v, ok := n.Data.Attr("name"); ok && v == name {
				out = append(out, cur)
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())
	return out
}

func (doc *Document) Title() string {
	titles := doc.GetElementsByTagName("title")
	if len(titles) == 0 {
		return ""
	}
	return strings.TrimSpace(doc.TextContent(titles[0]))
}

func (doc *Document) StyleSheets() []StyleSheetSource {
	var out []StyleSheetSource
	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement {
			switch n.Data.Tag {
			case "style":
				media, _ := n.Data.Attr("media")
				out = append(out, StyleSheetSource{
					Kind:  StyleSheetInline,
					CSS:   doc.TextContent(cur),
					Media: strings.TrimSpace(media),
				})
			case "link":
				rel, _ := n.Data.Attr("rel")
				href, _ := n.Data.Attr("href")
				if strings.Contains(strings.ToLower(rel), "stylesheet") && strings.TrimSpace(href) != "" {
					media, _ := n.Data.Attr("media")
					integrity, _ := n.Data.Attr("integrity")
					out = append(out, StyleSheetSource{
						Kind:      StyleSheetLink,
						Href:      strings.TrimSpace(href),
						Integrity: strings.TrimSpace(integrity),
						Media:     strings.TrimSpace(media),
					})
				}
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())
	return out
}

// Shadow DOM & Slot Projection
func (doc *Document) AttachShadow(host NodeId, open bool) (NodeId, error) {
	n := doc.Node(host)
	if n == nil || n.Data.Kind != NodeKindElement {
		return 0, fmt.Errorf("host is not an element")
	}
	if _, exists := doc.shadowRoots[host]; exists {
		return 0, fmt.Errorf("shadow root already attached")
	}
	sr := doc.CreateElement("#shadow-root")
	doc.shadowRoots[host] = ShadowRootInfo{Root: sr, Open: open}
	doc.shadowHosts[sr] = host
	doc.markDirty(host)
	return sr, nil
}

func (doc *Document) ShadowRoot(host NodeId) (NodeId, bool, bool) {
	info, ok := doc.shadowRoots[host]
	if !ok {
		return 0, false, false
	}
	return info.Root, info.Open, true
}

func (doc *Document) ShadowHost(node NodeId) (NodeId, bool) {
	cur := node
	for {
		if host, ok := doc.shadowHosts[cur]; ok {
			return host, true
		}
		p, ok := doc.Parent(cur)
		if !ok {
			return 0, false
		}
		cur = p
	}
}

func (doc *Document) AssignedNodes(slot NodeId) []NodeId {
	sn := doc.Node(slot)
	if sn == nil || sn.Data.Tag != "slot" {
		return nil
	}
	host, ok := doc.ShadowHost(slot)
	if !ok {
		return nil
	}
	slotName, _ := sn.Data.Attr("name")
	var assigned []NodeId
	for _, c := range doc.Children(host) {
		cn := doc.Node(c)
		if cn == nil {
			continue
		}
		childSlot, _ := cn.Data.Attr("slot")
		if childSlot == slotName {
			assigned = append(assigned, c)
		}
	}
	if len(assigned) == 0 {
		return sn.Children
	}
	return assigned
}

func (doc *Document) AssignedElements(slot NodeId) []NodeId {
	var elems []NodeId
	for _, id := range doc.AssignedNodes(slot) {
		if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
			elems = append(elems, id)
		}
	}
	return elems
}

func (doc *Document) ComposedChildren(id NodeId) []NodeId {
	if sr, ok := doc.shadowRoots[id]; ok {
		return doc.expandSlots(sr.Root)
	}
	n := doc.Node(id)
	if n == nil {
		return nil
	}
	if n.Data.Tag == "slot" {
		return doc.AssignedNodes(id)
	}
	return n.Children
}

func (doc *Document) expandSlots(root NodeId) []NodeId {
	var out []NodeId
	for _, c := range doc.Children(root) {
		cn := doc.Node(c)
		if cn != nil && cn.Data.Tag == "slot" {
			out = append(out, doc.AssignedNodes(c)...)
		} else {
			out = append(out, c)
		}
	}
	return out
}

// Interactive states, scroll offsets, and form constraint validation
func (doc *Document) SetHoveredNode(id *NodeId)  { doc.hoveredNode = id }
func (doc *Document) HoveredNode() *NodeId       { return doc.hoveredNode }
func (doc *Document) SetActiveNode(id *NodeId)   { doc.activeNode = id }
func (doc *Document) ActiveNode() *NodeId        { return doc.activeNode }
func (doc *Document) SetFocusedNode(id *NodeId)  { doc.focusedNode = id }
func (doc *Document) FocusedNode() *NodeId       { return doc.focusedNode }
func (doc *Document) SetTargetFragment(f string) { doc.targetFragment = strings.TrimPrefix(f, "#") }
func (doc *Document) TargetFragment() string     { return doc.targetFragment }

func (doc *Document) NodeScrollOffset(id NodeId) (uint32, uint32) {
	off := doc.scrollOffsets[id]
	return off.X, off.Y
}

func (doc *Document) SetNodeScrollOffset(id NodeId, x, y uint32) {
	doc.scrollOffsets[id] = ScrollOffset{X: x, Y: y}
}

func (doc *Document) SetCustomValidity(id NodeId, msg string) {
	if msg == "" {
		delete(doc.customValidity, id)
	} else {
		doc.customValidity[id] = msg
	}
}

func (doc *Document) CustomValidity(id NodeId) string {
	return doc.customValidity[id]
}

func (doc *Document) IsControlValid(id NodeId) bool {
	if msg := doc.customValidity[id]; msg != "" {
		return false
	}
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return true
	}
	val, _ := n.Data.Attr("value")
	if _, req := n.Data.Attr("required"); req {
		if n.Data.Tag == "input" {
			tp, _ := n.Data.Attr("type")
			if strings.EqualFold(tp, "checkbox") || strings.EqualFold(tp, "radio") {
				_, checked := n.Data.Attr("checked")
				return checked
			}
		}
		if strings.TrimSpace(val) == "" && strings.TrimSpace(doc.TextContent(id)) == "" {
			return false
		}
	}
	return doc.IsControlInRange(id)
}

func (doc *Document) IsControlInRange(id NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return true
	}
	valStr, ok := n.Data.Attr("value")
	if !ok || strings.TrimSpace(valStr) == "" {
		return true
	}
	v, err := strconv.ParseFloat(strings.TrimSpace(valStr), 64)
	if err != nil {
		return true
	}
	if minStr, ok := n.Data.Attr("min"); ok {
		if minV, err := strconv.ParseFloat(strings.TrimSpace(minStr), 64); err == nil && v < minV {
			return false
		}
	}
	if maxStr, ok := n.Data.Attr("max"); ok {
		if maxV, err := strconv.ParseFloat(strings.TrimSpace(maxStr), 64); err == nil && v > maxV {
			return false
		}
	}
	return true
}

func (doc *Document) EnsureCanvasSurface(id NodeId, w, h uint32) *CanvasSurface {
	if w == 0 {
		w = 300
	}
	if h == 0 {
		h = 150
	}
	if surf, ok := doc.canvasSurfaces[id]; ok && surf.Width == w && surf.Height == h {
		return surf
	}
	surf := &CanvasSurface{
		Width:  w,
		Height: h,
		RGBA:   make([]byte, int(w*h*4)),
	}
	doc.canvasSurfaces[id] = surf
	return surf
}

func (doc *Document) CanvasSurface(id NodeId) (*CanvasSurface, bool) {
	surf, ok := doc.canvasSurfaces[id]
	return surf, ok
}
