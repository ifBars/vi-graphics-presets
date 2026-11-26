# VI Graphics Presets

An experimental graphics-preset plugin for native VIctory-family runtimes.

It changes selected title settings after the game loads its visual configuration and before the game publishes that table. Presets use the active Series S or Series X row when one exists, then fall back to the shared row.

## Current status

The first supported build is the Xbox 1.0.2.1 executable, identified by its original BLAKE3 hash. The included adapter has been tested with ScarlettPort 0.4.3 on Windows 10, an RTX 3060 Ti 8 GB, and NVIDIA driver 621.40.

Verified so far:

- Ten CPU tests pass, including the authored Win64 call-relay test and the complete eight-setting experimental plan.
- A bounded title dry run found the real visual table and planned changes without writing them.
- The applying run changed `imposter.trees.SLODDistanceScale` from 1.0 to 0.75. A separate read-only process check confirmed 0.75 in the title's consuming global; the unmodified imposter-distance global remained 1.0.
- The expanded inspect preset left all eight consumer globals at their original values. The aggressive preset then changed all eight to the values below, confirmed by independent read-only checks of those globals. Its Story rendering and FPS comparison are pending.

The opening Story world renders. The first test had dark surfaces and missing objects, with about 3 FPS reported (1–7 FPS), and stopped at its physical-memory guard. A later run with a smaller runtime render-target reserve completed its time limit; the tester saw substantially fewer dark surfaces and missing objects as the sequence progressed. That run also had a warmer pipeline cache, so neither the rendering change nor a small measured FPS difference can be attributed to the preset alone. Both configurations are preserved locally. Other forks, other game builds, Windows 11, and AMD hardware remain untested. This is not a full Low graphics preset.

## Presets

| File | Changes |
| --- | --- |
| `inspect-tree-distance.json` | Plans both tree-distance changes and logs them, with no table writes. |
| `smoke-tree-distance.json` | Multiplies tree SLOD distance by 0.75. |
| `low-tree-distance.json` | Multiplies tree SLOD and imposter distances by 0.75. |
| `inspect-aggressive-distance.json` | Plans the eight experimental changes below without table writes. |
| `aggressive-distance.json` | Multiplies eight distance and coverage settings by 0.5. Experimental; workload reduction is unverified. |

For example:

```json
{
  "schema_version": 1,
  "id": "smoke-tree-distance",
  "dry_run": false,
  "settings": [
    { "key": "imposter.trees.SLODDistanceScale", "multiply": 0.75 }
  ]
}
```

Version 1 accepts the following keys, with finite multipliers from 0.25 through 1.0. The multiplier applies to the effective platform value. Unsupported keys and ambiguous or missing rows reject the complete plan.

| Key | Shared default → aggressive value | Scope |
| --- | --- | --- |
| `imposter.trees.imposterDistScale` | 1 → 0.5 | Tree imposter distance scale. |
| `imposter.trees.SLODDistanceScale` | 1 → 0.5 | Tree SLOD distance scale. |
| `terrain.approximate_vegetation.distance_max` | 50 → 25 | Approximate terrain vegetation fade. |
| `terrain.approximate_vegetation.grass.distance_max` | 20 → 10 | Approximate terrain grass fade. |
| `tessellation.water.near` | 52 → 26 | Water tessellation threshold; paired with `far`. |
| `tessellation.water.far` | 55 → 27.5 | Water tessellation threshold; paired with `near`. |
| `misc.fog.distance_coverage` | 22,000 → 11,000 | Fog renderer coverage constants. |
| `lights.cutoff.volumetric.distance` | 1,003 → 501.5 | Default volumetric light cutoff; individual lights may override it. |

These keys have traced scalar consumers in the supported executable. Vegetation fades affect approximate terrain rendering, rather than all vegetation. Moving water thresholds or reducing fog coverage may change appearance without reducing work. Keep the authored water pair together. Check the log for actual values: platform rows can differ from these shared defaults. Shadow settings and terrain geometry transitions remain outside the allowlist.

## Build and run

Build on Windows x64 with the Rust MSVC toolchain:

```powershell
cargo test --locked --lib
cargo build --release --locked
```

The plugin is `target\release\vi_graphics_presets.dll`. A runtime needs the small loader adapter; this is not a drop-in DLL for every fork. The ScarlettPort adapter patch is in [`adapters/scarlettport`](adapters/scarlettport).

With that adapter built, set absolute paths before your usual runtime command:

```powershell
$env:VICTORY_GRAPHICS_PRESET_DLL = 'D:\Mods\VI-Graphics-Presets\vi_graphics_presets.dll'
$env:VICTORY_GRAPHICS_PRESET = 'D:\Mods\VI-Graphics-Presets\presets\inspect-tree-distance.json'
$env:VICTORY_GRAPHICS_PRESET_LOG = 'D:\Logs\preset-inspect.jsonl'
# Start your runtime with its recommended game, cache and profile settings.
```

Create the log's parent directory first. Start with the inspect preset. `planned` records contain the selected row, original value and proposed value. An applying preset emits `applied` only after value readback succeeds. `refused` means the preset was not accepted; do not count that run as a preset comparison.

Clear all three variables for an ordinary run. Changes exist only in the running title's memory. Each new title launch reads its original configuration again.

## Fork integration

The plugin has a versioned Windows ABI, [`include/vi_graphics_presets.h`](include/vi_graphics_presets.h). The loader provides the mapped image base, size, original executable hash, preset path and log path before title execution. It retains the DLL for the process lifetime. The plugin checks the build and exact call site, invokes the original parser once, and keeps its return value.

The DLL and preset files are separate from the runtime adapter. Other native forks can add an adapter without copying this preset planner into their renderer. Each integration still needs its own test; support is currently verified only for the ScarlettPort adapter above.

## Comparing a preset

Keep the runtime, driver, console profile, save, output size and prepared-cache provenance together. Use the same scene for the baseline and preset runs. Record the effective values, rendering differences, actual frame intervals, compile stalls and resource limits. A lower setting value alone does not establish a speed gain.

## License and credits

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE.md](NOTICE.md). This repository contains plugin source, adapter source, authored presets and compatibility metadata. Users supply their own game files and runtime.

AI assistance: Codex / GPT-6-family agents helped inspect the local build, implement the prototype, and run the recorded checks. The repository owner is responsible for the changes.
