# Security

Private reporting isn't switched on for this repository or `anvil-browser` yet, so please use the private advisory form on one of the Anvil library repositories, for example `anvil-json`, and say which repository or engine component the problem is in:

https://github.com/buntatoes/anvil-json/security/advisories/new

Do not put the details in a public issue.

## Where the limits are

The DOM (`MaxDomDepth = 256`, `MaxDomNodes = 50000`, `MaxAttrsPerElement = 128`), stylesheet (`MaxSelectorNesting = 64`, `MaxSelectorCompounds = 640`), and script/WASM (`MaxExecutionSteps = 200000`, `MaxScriptSourceLen = 256 KiB`, `MaxWasmSteps = 5000000`, `MaxWasmPages = 2560`) limits are enforced in pure Go across `dom`, `style`, `layout`, `paint`, `raster`, and `script`.

Version 0.24.5.1 is current.
