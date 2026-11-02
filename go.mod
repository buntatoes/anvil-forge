// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0-OR-Commercial
// Copyright (c) 2026 Buntato

module github.com/buntatoes/anvil-engine

go 1.22

require (
	github.com/buntatoes/anvil-compress v0.4.0
	github.com/buntatoes/anvil-image v0.9.0
	github.com/buntatoes/anvil-ttf v0.11.0
)

replace (
	github.com/buntatoes/anvil-compress => ../anvil-compress
	github.com/buntatoes/anvil-image => ../anvil-image
	github.com/buntatoes/anvil-ttf => ../anvil-ttf
)
