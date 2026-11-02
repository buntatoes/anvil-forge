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

// Document-level attribute and class mutations that record dirty-subtree invalidation

func (doc *Document) SetAttribute(id NodeId, name, value string) {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		n.Data.SetAttr(name, value)
		doc.markDirty(id)
	}
}

func (doc *Document) RemoveAttribute(id NodeId, name string) bool {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		if n.Data.RemoveAttr(name) {
			doc.markDirty(id)
			return true
		}
	}
	return false
}

func (doc *Document) AddClass(id NodeId, className string) {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		n.Data.AddClass(className)
		doc.markDirty(id)
	}
}

func (doc *Document) RemoveClass(id NodeId, className string) {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		n.Data.RemoveClass(className)
		doc.markDirty(id)
	}
}

func (doc *Document) ToggleClass(id NodeId, className string) bool {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		added := n.Data.ToggleClass(className)
		doc.markDirty(id)
		return added
	}
	return false
}

func (doc *Document) HasDirtyStyleNodes() bool {
	return len(doc.dirtyNodes) > 0
}

func (doc *Document) ClearDirtyStyleNodes() {
	doc.dirtyNodes = nil
}

func (doc *Document) ParentElement(id NodeId) (NodeId, bool) {
	cur := id
	for {
		p, ok := doc.Parent(cur)
		if !ok {
			if host, okHost := doc.shadowHosts[cur]; okHost {
				return host, true
			}
			return 0, false
		}
		if n := doc.Node(p); n != nil && n.Data.Kind == NodeKindElement {
			return p, true
		}
		cur = p
	}
}

func (doc *Document) ContainsNode(ancestor, descendant NodeId) bool {
	if ancestor == descendant {
		return true
	}
	cur := descendant
	for {
		p, ok := doc.Parent(cur)
		if !ok {
			if host, okHost := doc.shadowHosts[cur]; okHost {
				p = host
				ok = true
			}
		}
		if !ok {
			return false
		}
		if p == ancestor {
			return true
		}
		cur = p
	}
}

func (doc *Document) CoalesceDirtySubtreeRoots() []NodeId {
	if len(doc.dirtyNodes) == 0 {
		return nil
	}
	var promoted []NodeId
	for _, d := range doc.dirtyNodes {
		n := doc.Node(d)
		if n == nil {
			continue
		}
		elemID := d
		if n.Data.Kind != NodeKindElement {
			if pe, ok := doc.ParentElement(d); ok {
				elemID = pe
			} else {
				elemID = doc.Body()
			}
		}
		exists := false
		for _, ex := range promoted {
			if ex == elemID {
				exists = true
				break
			}
		}
		if !exists {
			promoted = append(promoted, elemID)
		}
	}
	var roots []NodeId
	for _, cand := range promoted {
		covered := false
		for _, anc := range promoted {
			if anc != cand && doc.ContainsNode(anc, cand) {
				covered = true
				break
			}
		}
		if !covered {
			roots = append(roots, cand)
		}
	}
	return roots
}

func (doc *Document) TakeCoalescedDirtySubtreeRoots() []NodeId {
	roots := doc.CoalesceDirtySubtreeRoots()
	doc.dirtyNodes = nil
	return roots
}

// Interactive Form & Search Submission Pipeline

const DefaultMultipartBoundary = "----AnvilFormBoundary7MA4YWxkTrZu0gW"

type FormFieldPair struct {
	Name  string
	Value string
}

type FormEntry struct {
	Name        string
	Value       string
	Filename    string
	ContentType string
	IsFile      bool
}

type FormData struct {
	entries []FormEntry
}

func NewFormData() *FormData {
	return &FormData{}
}

func NewFormDataFromForm(doc *Document, formID NodeId) *FormData {
	fd := &FormData{}
	if doc == nil {
		return fd
	}
	fd.entries = doc.CollectFormEntries(formID)
	return fd
}

func (fd *FormData) Append(name, value string) {
	if fd == nil {
		return
	}
	fd.entries = append(fd.entries, FormEntry{
		Name:  name,
		Value: value,
	})
}

func (fd *FormData) AppendFile(name, filename, contentType, content string) {
	if fd == nil {
		return
	}
	if strings.TrimSpace(contentType) == "" {
		contentType = "application/octet-stream"
	}
	fd.entries = append(fd.entries, FormEntry{
		Name:        name,
		Value:       content,
		Filename:    filename,
		ContentType: contentType,
		IsFile:      true,
	})
}

func (fd *FormData) Set(name, value string) {
	if fd == nil {
		return
	}
	replaced := false
	var next []FormEntry
	for _, e := range fd.entries {
		if e.Name == name {
			if !replaced {
				next = append(next, FormEntry{Name: name, Value: value})
				replaced = true
			}
			continue
		}
		next = append(next, e)
	}
	if !replaced {
		next = append(next, FormEntry{Name: name, Value: value})
	}
	fd.entries = next
}

func (fd *FormData) Get(name string) (string, bool) {
	if fd == nil {
		return "", false
	}
	for _, e := range fd.entries {
		if e.Name == name {
			if e.IsFile && e.Value == "" {
				return e.Filename, true
			}
			return e.Value, true
		}
	}
	return "", false
}

func (fd *FormData) GetAll(name string) []string {
	if fd == nil {
		return nil
	}
	var out []string
	for _, e := range fd.entries {
		if e.Name == name {
			if e.IsFile && e.Value == "" {
				out = append(out, e.Filename)
			} else {
				out = append(out, e.Value)
			}
		}
	}
	return out
}

func (fd *FormData) Has(name string) bool {
	_, ok := fd.Get(name)
	return ok
}

func (fd *FormData) Delete(name string) {
	if fd == nil {
		return
	}
	var next []FormEntry
	for _, e := range fd.entries {
		if e.Name != name {
			next = append(next, e)
		}
	}
	fd.entries = next
}

func (fd *FormData) Entries() []FormEntry {
	if fd == nil {
		return nil
	}
	cp := make([]FormEntry, len(fd.entries))
	copy(cp, fd.entries)
	return cp
}

func (fd *FormData) Keys() []string {
	if fd == nil {
		return nil
	}
	out := make([]string, len(fd.entries))
	for i, e := range fd.entries {
		out[i] = e.Name
	}
	return out
}

func (fd *FormData) Values() []string {
	if fd == nil {
		return nil
	}
	out := make([]string, len(fd.entries))
	for i, e := range fd.entries {
		if e.IsFile && e.Value == "" {
			out[i] = e.Filename
		} else {
			out[i] = e.Value
		}
	}
	return out
}

func (fd *FormData) EncodeURLEncoded() string {
	if fd == nil || len(fd.entries) == 0 {
		return ""
	}
	pairs := make([]FormFieldPair, 0, len(fd.entries))
	for _, e := range fd.entries {
		val := e.Value
		if e.IsFile && val == "" {
			val = e.Filename
		}
		pairs = append(pairs, FormFieldPair{Name: e.Name, Value: val})
	}
	return SerializeFormFields(pairs)
}

func escapeMultipartQuoted(s string) string {
	s = strings.ReplaceAll(s, "\\", "\\\\")
	s = strings.ReplaceAll(s, "\"", "\\\"")
	s = strings.ReplaceAll(s, "\r", "")
	s = strings.ReplaceAll(s, "\n", " ")
	return s
}

func (fd *FormData) EncodeMultipart(boundary string) (string, string) {
	if strings.TrimSpace(boundary) == "" {
		boundary = DefaultMultipartBoundary
	}
	contentType := "multipart/form-data; boundary=" + boundary
	if fd == nil {
		return "--" + boundary + "--\r\n", contentType
	}
	var b strings.Builder
	for _, e := range fd.entries {
		b.WriteString("--")
		b.WriteString(boundary)
		b.WriteString("\r\n")
		if e.IsFile || e.Filename != "" {
			fn := e.Filename
			ct := e.ContentType
			if strings.TrimSpace(ct) == "" {
				ct = "application/octet-stream"
			}
			b.WriteString("Content-Disposition: form-data; name=\"")
			b.WriteString(escapeMultipartQuoted(e.Name))
			b.WriteString("\"; filename=\"")
			b.WriteString(escapeMultipartQuoted(fn))
			b.WriteString("\"\r\nContent-Type: ")
			b.WriteString(ct)
			b.WriteString("\r\n\r\n")
			b.WriteString(e.Value)
			b.WriteString("\r\n")
		} else {
			b.WriteString("Content-Disposition: form-data; name=\"")
			b.WriteString(escapeMultipartQuoted(e.Name))
			b.WriteString("\"\r\n\r\n")
			b.WriteString(e.Value)
			b.WriteString("\r\n")
		}
	}
	b.WriteString("--")
	b.WriteString(boundary)
	b.WriteString("--\r\n")
	return b.String(), contentType
}

func SerializeFormMultipart(pairs []FormFieldPair, boundary string) (string, string) {
	fd := NewFormData()
	for _, p := range pairs {
		fd.Append(p.Name, p.Value)
	}
	return fd.EncodeMultipart(boundary)
}

type FormSubmissionRequest struct {
	ActionURL          string
	Method             string
	ContentType        string
	QueryOrBody        string
	FinalNavigationURL string
	Fields             []FormFieldPair
	Entries            []FormEntry
}

func UrlencodeComponent(s string) string {
	const hex = "0123456789ABCDEF"
	var b strings.Builder
	b.Grow(len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_' || c == '.' || c == '~':
			b.WriteByte(c)
		case c == ' ':
			b.WriteByte('+')
		default:
			b.WriteByte('%')
			b.WriteByte(hex[c>>4])
			b.WriteByte(hex[c&0x0F])
		}
	}
	return b.String()
}

func SerializeFormFields(pairs []FormFieldPair) string {
	if len(pairs) == 0 {
		return ""
	}
	var b strings.Builder
	for i, p := range pairs {
		if i > 0 {
			b.WriteByte('&')
		}
		b.WriteString(UrlencodeComponent(p.Name))
		b.WriteByte('=')
		b.WriteString(UrlencodeComponent(p.Value))
	}
	return b.String()
}

func ResolveFormAction(baseURL, action string) string {
	baseURL = strings.TrimSpace(baseURL)
	action = strings.TrimSpace(action)
	if action == "" {
		if idx := strings.IndexByte(baseURL, '#'); idx >= 0 {
			return baseURL[:idx]
		}
		return baseURL
	}
	lowerAct := strings.ToLower(action)
	if strings.HasPrefix(lowerAct, "http://") || strings.HasPrefix(lowerAct, "https://") {
		return action
	}
	schemeEnd := strings.Index(baseURL, "://")
	if schemeEnd < 0 {
		return action
	}
	scheme := baseURL[:schemeEnd]
	rest := baseURL[schemeEnd+3:]
	if strings.HasPrefix(action, "//") {
		return scheme + ":" + action
	}
	slashIdx := strings.IndexByte(rest, '/')
	origin := baseURL
	basePath := "/"
	if slashIdx >= 0 {
		origin = baseURL[:schemeEnd+3+slashIdx]
		basePath = rest[slashIdx:]
		if qIdx := strings.IndexAny(basePath, "?#"); qIdx >= 0 {
			basePath = basePath[:qIdx]
		}
	}
	if strings.HasPrefix(action, "/") {
		return origin + action
	}
	if strings.HasPrefix(action, "?") {
		return origin + basePath + action
	}
	lastSlash := strings.LastIndexByte(basePath, '/')
	dir := "/"
	if lastSlash >= 0 {
		dir = basePath[:lastSlash+1]
	}
	return origin + dir + action
}

func AppendFormQuery(targetURL, encodedQuery string) string {
	base := targetURL
	frag := ""
	if hashIdx := strings.IndexByte(base, '#'); hashIdx >= 0 {
		frag = base[hashIdx:]
		base = base[:hashIdx]
	}
	if qIdx := strings.IndexByte(base, '?'); qIdx >= 0 {
		base = base[:qIdx]
	}
	if encodedQuery == "" {
		return base + frag
	}
	return base + "?" + encodedQuery + frag
}

func (doc *Document) IsTextInput(id NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return false
	}
	switch n.Data.Tag {
	case "textarea":
		return true
	case "input":
		tp, ok := n.Data.Attr("type")
		if !ok {
			return true
		}
		switch strings.ToLower(strings.TrimSpace(tp)) {
		case "", "text", "search", "password", "email", "url", "tel", "number":
			return true
		}
	}
	return false
}

func (doc *Document) IsSubmitControl(id NodeId) bool {
	n := doc.Node(id)
	if n == nil || n.Data.Kind != NodeKindElement {
		return false
	}
	switch n.Data.Tag {
	case "input":
		tp, _ := n.Data.Attr("type")
		return strings.EqualFold(strings.TrimSpace(tp), "submit")
	case "button":
		tp, ok := n.Data.Attr("type")
		if !ok {
			return true
		}
		return strings.EqualFold(strings.TrimSpace(tp), "submit")
	}
	return false
}

func (doc *Document) EnclosingForm(id NodeId) (NodeId, bool) {
	if n := doc.Node(id); n != nil && n.Data.Kind == NodeKindElement {
		if formAttr, ok := n.Data.Attr("form"); ok && strings.TrimSpace(formAttr) != "" {
			if formNode, found := doc.GetElementByID(strings.TrimSpace(formAttr)); found {
				if fn := doc.Node(formNode); fn != nil && fn.Data.Tag == "form" {
					return formNode, true
				}
			}
		}
	}
	cur := id
	for {
		n := doc.Node(cur)
		if n == nil {
			return 0, false
		}
		if n.Data.Kind == NodeKindElement && n.Data.Tag == "form" {
			return cur, true
		}
		p, ok := doc.Parent(cur)
		if !ok {
			return 0, false
		}
		cur = p
	}
}

func (doc *Document) CollectFormEntries(formID NodeId) []FormEntry {
	var entries []FormEntry
	formNode := doc.Node(formID)
	if formNode == nil || formNode.Data.Tag != "form" {
		return nil
	}
	formAttrID := formNode.Data.ID()

	collectControl := func(cid NodeId) {
		cn := doc.Node(cid)
		if cn == nil || cn.Data.Kind != NodeKindElement {
			return
		}
		if _, disabled := cn.Data.Attr("disabled"); disabled {
			return
		}
		name, ok := cn.Data.Attr("name")
		name = strings.TrimSpace(name)
		if !ok || name == "" {
			return
		}
		switch cn.Data.Tag {
		case "input":
			tp, _ := cn.Data.Attr("type")
			tp = strings.ToLower(strings.TrimSpace(tp))
			switch tp {
			case "submit", "button", "reset", "image":
				return
			case "file":
				fn, hasFn := cn.Data.Attr("filename")
				if !hasFn {
					fn, hasFn = cn.Data.Attr("data-filename")
				}
				val, hasVal := cn.Data.Attr("value")
				if !hasFn && hasVal {
					fn = val
				}
				ct, _ := cn.Data.Attr("data-content-type")
				if strings.TrimSpace(ct) == "" {
					ct = "application/octet-stream"
				}
				content, hasContent := cn.Data.Attr("data-file-content")
				if !hasContent {
					content = val
				}
				entries = append(entries, FormEntry{
					Name:        name,
					Value:       content,
					Filename:    strings.TrimSpace(fn),
					ContentType: strings.TrimSpace(ct),
					IsFile:      true,
				})
			case "checkbox", "radio":
				if _, checked := cn.Data.Attr("checked"); !checked {
					return
				}
				val, hasVal := cn.Data.Attr("value")
				if !hasVal {
					val = "on"
				}
				entries = append(entries, FormEntry{Name: name, Value: val})
			default:
				val, _ := cn.Data.Attr("value")
				entries = append(entries, FormEntry{Name: name, Value: val})
			}
		case "textarea":
			val, hasVal := cn.Data.Attr("value")
			if !hasVal {
				val = doc.TextContent(cid)
			}
			entries = append(entries, FormEntry{Name: name, Value: val})
		case "select":
			selectedVal := ""
			foundSelected := false
			var firstVal string
			hasFirst := false
			for _, oid := range cn.Children {
				on := doc.Node(oid)
				if on == nil || on.Data.Tag != "option" {
					continue
				}
				v, hasV := on.Data.Attr("value")
				if !hasV {
					v = strings.TrimSpace(doc.TextContent(oid))
				}
				if !hasFirst {
					firstVal = v
					hasFirst = true
				}
				if selAttr, hasSel := on.Data.Attr("selected"); hasSel && !strings.EqualFold(selAttr, "false") {
					selectedVal = v
					foundSelected = true
					break
				}
			}
			if !foundSelected && hasFirst {
				selectedVal = firstVal
				foundSelected = true
			}
			if foundSelected {
				entries = append(entries, FormEntry{Name: name, Value: selectedVal})
			}
		}
	}

	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if cur != formID && n.Data.Kind == NodeKindElement {
			owner, hasOwner := doc.EnclosingForm(cur)
			if hasOwner && owner == formID {
				collectControl(cur)
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	if formAttrID != "" {
		walk(doc.Root())
	} else {
		walk(formID)
	}
	return entries
}

func (doc *Document) CollectFormFieldPairs(formID NodeId) []FormFieldPair {
	var pairs []FormFieldPair
	for _, e := range doc.CollectFormEntries(formID) {
		if e.IsFile {
			continue
		}
		pairs = append(pairs, FormFieldPair{Name: e.Name, Value: e.Value})
	}
	return pairs
}

func normalizeFormEnctype(raw string) string {
	switch strings.ToLower(strings.TrimSpace(raw)) {
	case "multipart/form-data":
		return "multipart/form-data"
	case "text/plain":
		return "text/plain"
	default:
		return "application/x-www-form-urlencoded"
	}
}

func (doc *Document) FormEnctype(formID NodeId, submitterID NodeId) string {
	if submitterID != 0 {
		if sn := doc.Node(submitterID); sn != nil && sn.Data.Kind == NodeKindElement {
			if fe, ok := sn.Data.Attr("formenctype"); ok && strings.TrimSpace(fe) != "" {
				return normalizeFormEnctype(fe)
			}
		}
	}
	if fn := doc.Node(formID); fn != nil && fn.Data.Tag == "form" {
		if enc, ok := fn.Data.Attr("enctype"); ok && strings.TrimSpace(enc) != "" {
			return normalizeFormEnctype(enc)
		}
	}
	return "application/x-www-form-urlencoded"
}

func (doc *Document) SerializeForm(formID NodeId) (action, method, encoded string) {
	fn := doc.Node(formID)
	if fn == nil || fn.Data.Tag != "form" {
		return "", "GET", ""
	}
	act, _ := fn.Data.Attr("action")
	action = strings.TrimSpace(act)
	m, _ := fn.Data.Attr("method")
	if strings.EqualFold(strings.TrimSpace(m), "POST") {
		method = "POST"
	} else {
		method = "GET"
	}
	pairs := doc.CollectFormFieldPairs(formID)
	encoded = SerializeFormFields(pairs)
	return action, method, encoded
}

func (doc *Document) BuildFormSubmission(pageURL string, controlID NodeId) (*FormSubmissionRequest, bool) {
	formID, ok := doc.EnclosingForm(controlID)
	if !ok {
		return nil, false
	}
	action, method, _ := doc.SerializeForm(formID)
	entries := doc.CollectFormEntries(formID)
	pairs := doc.CollectFormFieldPairs(formID)

	var submitter NodeId
	hasSubmitter := false
	if doc.IsSubmitControl(controlID) {
		if n := doc.Node(controlID); n != nil {
			if _, disabled := n.Data.Attr("disabled"); !disabled {
				submitter = controlID
				hasSubmitter = true
			}
		}
	} else {
		var findFirstSubmit func(NodeId) bool
		findFirstSubmit = func(cur NodeId) bool {
			if doc.IsSubmitControl(cur) {
				if n := doc.Node(cur); n != nil {
					if _, disabled := n.Data.Attr("disabled"); !disabled {
						submitter = cur
						hasSubmitter = true
						return true
					}
				}
			}
			n := doc.Node(cur)
			if n == nil {
				return false
			}
			for _, c := range n.Children {
				if findFirstSubmit(c) {
					return true
				}
			}
			return false
		}
		findFirstSubmit(formID)
	}

	if hasSubmitter {
		if sn := doc.Node(submitter); sn != nil {
			if fa, ok := sn.Data.Attr("formaction"); ok && strings.TrimSpace(fa) != "" {
				action = strings.TrimSpace(fa)
			}
			if fm, ok := sn.Data.Attr("formmethod"); ok && strings.TrimSpace(fm) != "" {
				if strings.EqualFold(strings.TrimSpace(fm), "POST") {
					method = "POST"
				} else {
					method = "GET"
				}
			}
			if sName, ok := sn.Data.Attr("name"); ok && strings.TrimSpace(sName) != "" {
				sVal, _ := sn.Data.Attr("value")
				trimmedName := strings.TrimSpace(sName)
				pairs = append(pairs, FormFieldPair{
					Name:  trimmedName,
					Value: sVal,
				})
				entries = append(entries, FormEntry{
					Name:  trimmedName,
					Value: sVal,
				})
			}
		}
	}

	enctype := doc.FormEnctype(formID, submitter)
	encoded := SerializeFormFields(pairs)
	contentType := "application/x-www-form-urlencoded"
	queryOrBody := encoded
	if method == "POST" && enctype == "multipart/form-data" {
		fd := &FormData{entries: entries}
		queryOrBody, contentType = fd.EncodeMultipart(DefaultMultipartBoundary)
	}
	resolvedAction := ResolveFormAction(pageURL, action)
	req := &FormSubmissionRequest{
		ActionURL:   resolvedAction,
		Method:      method,
		ContentType: contentType,
		QueryOrBody: queryOrBody,
		Fields:      pairs,
		Entries:     entries,
	}
	if method == "POST" {
		req.FinalNavigationURL = resolvedAction
	} else {
		req.FinalNavigationURL = AppendFormQuery(resolvedAction, encoded)
	}
	return req, true
}

// Prioritized Subresource Discovery for HTTP/2 Stream Multiplexing

type SubresourcePriority int

const (
	PriorityCriticalStyle SubresourcePriority = iota
	PriorityHighFont
	PriorityBlockingScript
	PriorityAboveFoldImage
	PriorityAsyncScript
	PriorityLazyMedia
)

func (p SubresourcePriority) Weight() uint8 {
	switch p {
	case PriorityCriticalStyle:
		return 255
	case PriorityHighFont:
		return 220
	case PriorityBlockingScript:
		return 192
	case PriorityAboveFoldImage:
		return 128
	case PriorityAsyncScript:
		return 96
	default:
		return 32
	}
}

func (p SubresourcePriority) Urgency() uint8 {
	switch p {
	case PriorityCriticalStyle:
		return 0
	case PriorityHighFont, PriorityBlockingScript:
		return 1
	case PriorityAboveFoldImage:
		return 2
	case PriorityAsyncScript:
		return 3
	default:
		return 5
	}
}

type SubresourceDescriptor struct {
	URL         string
	Kind        string
	Priority    SubresourcePriority
	Integrity   string
	CrossOrigin string
}

func (doc *Document) DiscoverPrioritizedSubresources(baseURL string) []SubresourceDescriptor {
	var list []SubresourceDescriptor
	seen := make(map[string]bool)

	add := func(rawHref, kind string, prio SubresourcePriority, integrity, crossOrigin string) {
		rawHref = strings.TrimSpace(rawHref)
		if rawHref == "" || strings.HasPrefix(strings.ToLower(rawHref), "data:") || strings.HasPrefix(strings.ToLower(rawHref), "javascript:") {
			return
		}
		resolved := ResolveFormAction(baseURL, rawHref)
		if resolved == "" || seen[resolved] {
			return
		}
		seen[resolved] = true
		list = append(list, SubresourceDescriptor{
			URL:         resolved,
			Kind:        kind,
			Priority:    prio,
			Integrity:   strings.TrimSpace(integrity),
			CrossOrigin: strings.TrimSpace(crossOrigin),
		})
	}

	var walk func(NodeId)
	walk = func(cur NodeId) {
		n := doc.Node(cur)
		if n == nil {
			return
		}
		if n.Data.Kind == NodeKindElement {
			integrity, _ := n.Data.Attr("integrity")
			crossOrigin, _ := n.Data.Attr("crossorigin")
			switch n.Data.Tag {
			case "link":
				rel, _ := n.Data.Attr("rel")
				relLower := strings.ToLower(rel)
				href, _ := n.Data.Attr("href")
				if strings.Contains(relLower, "preload") {
					asAttr, _ := n.Data.Attr("as")
					switch strings.ToLower(strings.TrimSpace(asAttr)) {
					case "style":
						add(href, "style", PriorityCriticalStyle, integrity, crossOrigin)
					case "font":
						add(href, "font", PriorityHighFont, integrity, crossOrigin)
					case "script":
						add(href, "script", PriorityBlockingScript, integrity, crossOrigin)
					case "image":
						add(href, "image", PriorityAboveFoldImage, integrity, crossOrigin)
					}
				} else if strings.Contains(relLower, "stylesheet") {
					media, _ := n.Data.Attr("media")
					prio := PriorityCriticalStyle
					if strings.EqualFold(strings.TrimSpace(media), "print") {
						prio = PriorityLazyMedia
					}
					add(href, "style", prio, integrity, crossOrigin)
				}
			case "script":
				if src, ok := n.Data.Attr("src"); ok {
					_, isAsync := n.Data.Attr("async")
					_, isDefer := n.Data.Attr("defer")
					prio := PriorityBlockingScript
					if isAsync || isDefer {
						prio = PriorityAsyncScript
					}
					add(src, "script", prio, integrity, crossOrigin)
				}
			case "img":
				if src, ok := n.Data.Attr("src"); ok {
					loading, _ := n.Data.Attr("loading")
					prio := PriorityAboveFoldImage
					if strings.EqualFold(strings.TrimSpace(loading), "lazy") {
						prio = PriorityLazyMedia
					}
					add(src, "image", prio, integrity, crossOrigin)
				}
			}
		}
		for _, c := range n.Children {
			walk(c)
		}
	}
	walk(doc.Root())

	// Stable priority sort (highest weight / lowest urgency value first)
	for i := 1; i < len(list); i++ {
		for j := i; j > 0 && list[j].Priority < list[j-1].Priority; j-- {
			list[j], list[j-1] = list[j-1], list[j]
		}
	}
	return list
}
