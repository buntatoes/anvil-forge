<div align="center">
  <h1>Anvil Forge Engine</h1>
  <p>Pure-Go page pipeline for Anvil Browser. Not the window, the network stack, or the profile file.</p>
  <p>
    <img src="https://img.shields.io/badge/release-v.24.5.1-ff6b24?style=flat-square" alt="v.24.5.1" />
    <img src="https://img.shields.io/badge/status-Proof_of_Concept-d4a359?style=flat-square" alt="Proof of Concept" />
    <a href="https://github.com/buntatoes/anvil-engine/actions/workflows/ci.yml"><img src="https://github.com/buntatoes/anvil-engine/actions/workflows/ci.yml/badge.svg" alt="CI: go vet, build, and tests" /></a>
    <img src="https://img.shields.io/badge/external_modules-0_%28offline_verified%29-2ea44f?style=flat-square" alt="0 external modules (offline verified)" />
    <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Noncommercial_%28based_on_PolyForm_1.0.0%29_%7C_Paid_Commercial_%28Non--Compete%29-blue?style=flat-square" alt="Noncommercial (based on PolyForm 1.0.0) | Paid Commercial (Non-Compete)" /></a>
    <img src="https://img.shields.io/badge/go-1.22+-00ADD8?style=flat-square" alt="Go 1.22+" />
  </p>
</div>

Version 0.24.5.1 (`v.24.5.1`). Author: Buntato.

This repository is part of the [Anvil Browser](https://github.com/buntatoes/anvil-browser) ecosystem and is dual-licensed under **noncommercial terms based on the PolyForm Noncommercial License 1.0.0** and a **Paid Commercial License with a Non-Compete Clause**. See [License](#license).

100% pure Go (`module github.com/buntatoes/anvil-engine`) with zero external module dependencies.

## Pipeline

| Package | What it is |
| --- | --- |
| `dom` | HTML parser (`dom.ParseHTML`) with WHATWG HTML5 error recovery (active formatting element reconstruction across misnested inline tags, table foster parenting, implicit heading/paragraph/list/table auto-closure, and foreign `<svg>`/`<math>` self-closing tags), official engine identity constants (`RenderingEngineName = "Forge"`, `JsEngineName = "Cinder"`, `CssEngineName = "Prism"`), and DOM tree: `<noscript>` isolation, sibling/child traversal, `GetElementsByTagName` / `GetElementsByClassName` / `GetElementsByName`, per-element scroll offsets (`NodeScrollOffset`, `SetNodeScrollOffset`), custom form constraint messages (`CustomValidity`, `SetCustomValidity`, `IsControlValid`, `IsControlInRange`), interactive state tracking (`HoveredNode`, `ActiveNode`, `FocusedNode`, `TargetFragment`), dirty-bit style invalidation (`DirtyStyleNodes`, `TakeDirtyStyleNodes`), cross-document subtree adoption (`ImportSubtree`), Shadow DOM (`AttachShadow` with open/closed mode, `<slot>` projection via `ComposedChildren`, `AssignedNodes`, `AssignedElements`), Custom Elements, `<canvas>` 2D RGBA backing surfaces (`CanvasSurface`), and Selectors Level 4 matching (`>`, `+`, `~`, `:is()`, `:where()`, `:not()`, `:has()`, `:nth-child(An+B)`, `:first-child`, `:last-child`, `:empty`, `:checked`, `:disabled`, `:host`, `:defined`, `[attr^=...]`, `[attr$=...]`, `[attr*=...]`) |
| `style` | CSS parsing (`style.ParseCSS`, `style.ParseCSSWithMedia`), `@font-face` (`FontFaceRule`), `font-family` (`FontSansSerif`, `FontSerif`, `FontMonospace`), `text-transform` (`uppercase`, `lowercase`, `capitalize`), `letter-spacing`, `word-spacing`, `text-decoration: underline \| line-through`, `::before` and `::after` pseudo-element cascade (`content: "..." \| attr(...)`), CSS Nesting (`&` parent selector expansion and nested rule flattening), Selectors Level 4 (`:scope`, `:is()`, `:where()`, `:not()`, `:has()`, `:host`, and interactive/validation pseudo-classes `:hover`, `:active`, `:focus`, `:focus-visible`, `:focus-within`, `:target`, `:valid`, `:invalid`, `:in-range`, `:out-of-range`), CSS Color Level 4/5 (`#hex`, `rgb()`, `rgba()`, `hsl()`, `hsla()`, named colors), `linear-gradient()`, per-side border colors, `outline` and `outline-offset`, `object-fit` (`fill`, `contain`, `cover`, `none`, `scale-down`), `overflow: auto \| scroll` (`ScrollableOverflow`), `@media` and `@supports` queries, `@layer` cascade layers (with `!important` layer inversion), `@scope (<root>) to (<limit>)` scoped cascade boundaries with ancestor hop proximity, `@property` custom property registration, CSS-wide keywords (`initial`, `inherit`, `unset`, `revert`, `revert-layer`), custom properties (`--var` with recursive `var(--missing, fallback)` recovery), CSS Values Level 4 math (`calc()`, `min()`, `max()`, `clamp()`) and viewport/font units across box edges, `box-sizing: border-box`, flex/grid tracks and shorthands, 4-edge `auto` margins, keyframe animations, incremental dirty-subtree style invalidation (`ComputeStylesIncremental`), and parallel style resolution (`ComputeStylesParallel`) |
| `layout` | Composed-tree (`Document.ComposedChildren` across Shadow DOM `<slot>` projections), incremental layout caching (`LayoutCache` & `LayoutDocumentIncremental`), block, inline, `inline-block`, `::before` and `::after` generated content boxes, scrollable overflow containers (`overflow: auto \| scroll`, `FindScrollableContainer`), `FindNodeBox`, `FindNodeRect`, `object-fit` image sizing, `text-transform` / `letter-spacing` / `word-spacing` / `line-through` text layout, per-side `BorderColors`, `Outline`, flex (`row`, `row-reverse`, `column`, `column-reverse`, `wrap`, `column-gap`, `row-gap`, 4-edge `auto` margins), grid (`px`, `%`, `fr`, `grid-template-columns`, `grid-template-rows`, `gap`), and positioned layout (`relative`, `absolute`, `fixed`, `sticky`), `<canvas>` surface blitting, hit testing (`HitTest`, `HitTestScrolled`), and range text selection (`CollectTextRunsInRect`) |
| `paint` | Display list builder (`paint.BuildDisplayList` honoring W3C stacking context order `z-index < 0`, normal flow, `z-index: 0`, and `z-index > 0`, clips, transforms, opacity layers, sticky layers, `object-fit` image placement, per-side border colors, `line-through` and `underline` text decoration, 6px overlay scrollbars for `overflow: auto \| scroll`, and fixed layers) and page zoom (`DisplayList.Scaled`) |
| `raster` | RGBA framebuffer (`raster.Framebuffer` with fast row-slice blitting `CopyRowsFrom`, `ToBgraBottomUp`, analytical SDF rounded rectangles `FillRoundedRectSdf`, SDF box shadows `FillBoxShadowSdf`, linear gradients `FillLinearGradientSdf`, clipped image blitting `BlitImage`, 5x7 crisp bitmap text `DrawText`, and gamma-correct linear-space glyph alpha blending `BlendGlyphPixelGammaCorrect`), plus multi-core rasterization (`RasterEngine`) |
| `script` | Budgeted JavaScript and WebAssembly 1.0 runtime (`script.ScriptRuntime`) with DOM bindings (`getElementById`, `querySelector`, `textContent`, `innerHTML`, `setAttribute`, `removeAttribute`, `classList`, `addEventListener`), interactive event dispatch (`DispatchClick`, `DispatchInput`, `DispatchSubmit`, `DispatchKeydown`, `HydrateInteractiveBindings`), `localStorage`/`sessionStorage`, `fetch()` host callback (`SetFetchHost`, `RegisterFetchMock`), `CinderVmTelemetry`, and a self-contained WebAssembly 1.0 (`\0asm` version `1`) binary module decoder (`DecodeWasmModule`), validator (`ValidateWasm`), bounded 64 KiB page linear memory (`WasmMemory`, up to 2,560 pages / 160 MiB), and stack machine (`InstantiateWasm`, `CallExport`) |

`script` does not open a socket. `fetch()` is performed by the host in `anvil-browser`.

## Build

Go 1.22 or newer (`go1.27.3` verified). Zero third-party module dependencies.

```text
go test -v ./...
```

## The other repositories

The browser is [anvil-browser](https://github.com/buntatoes/anvil-browser) and this page engine is [anvil-engine](https://github.com/buntatoes/anvil-engine), built alongside the nine companion libraries below.

| Repository | What it is |
| --- | --- |
| [anvil-cipher](https://github.com/buntatoes/anvil-cipher) | AES-128, AES-256, CTR, AES-256-GCM, SHA-256, HMAC-SHA256, HKDF-SHA256 |
| [anvil-compress](https://github.com/buntatoes/anvil-compress) | DEFLATE, zlib, gzip |
| [anvil-hpack](https://github.com/buntatoes/anvil-hpack) | HPACK header compression for HTTP/2 |
| [anvil-image](https://github.com/buntatoes/anvil-image) | PNG, JPEG, GIF, WebP (`VP8L` + `VP8 `), ICO, BMP, PPM |
| [anvil-json](https://github.com/buntatoes/anvil-json) | JSON |
| [anvil-pqc](https://github.com/buntatoes/anvil-pqc) | ML-KEM-768, X25519, X25519MLKEM768, SHA-3 |
| [anvil-ttf](https://github.com/buntatoes/anvil-ttf) | TrueType and OpenType (`glyf`, `CFF `, `kern`, `GPOS`) |
| [anvil-gpu](https://github.com/buntatoes/anvil-gpu) | Compositor passes for boxes, shadows, gradients, and NV12/I420, on the CPU or a GPU |
| [anvil-video](https://github.com/buntatoes/anvil-video) | CPU frames (AVID/ADCT/VP8/VP9/AV1) and PCM/ADPCM/FLAC/MDCT/Opus audio in MP4, fMP4, and WebM containers |

## License

Dual-licensed under **noncommercial terms based on the PolyForm Noncommercial License 1.0.0** (free for personal, educational, academic, and noncommercial auditing/research use) and a **Paid Commercial License with a Non-Compete Clause** (required for any commercial, for-profit, or business use). Competing browser, rendering engine, JS VM, or codec use is prohibited without a signed commercial agreement from Buntato. See [LICENSE](./LICENSE).
