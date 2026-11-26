# ScarlettPort adapter

`graphics-preset-adapter.patch` adds the opt-in loader adapter to ScarlettPort 0.4.3 (`f0c262e`). It touches three loader files. The local test used that upstream revision plus separate SDK 620 and cache-reporting changes; those changes are not included in this patch.

Apply from a clean, isolated ScarlettPort checkout:

```powershell
git apply --check path\to\graphics-preset-adapter.patch
git apply path\to\graphics-preset-adapter.patch
```

Follow ScarlettPort's own build, Agility SDK, shader and pipeline preparation instructions. Then use the three absolute-path environment variables in the main README.

The adapter uses `LoadLibraryExW` with an absolute DLL path and resolves `gp_install_v1`. Imported title DLLs follow their existing path. Installation errors stop the main title launch, and the loader retains the plugin reference until process exit.
