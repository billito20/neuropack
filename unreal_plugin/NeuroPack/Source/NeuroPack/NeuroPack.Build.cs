// Copyright NeuroPack Team. All Rights Reserved.
// NeuroPack.Build.cs — Unreal Build Tool module rules.

using UnrealBuildTool;
using System.IO;

public class NeuroPack : ModuleRules
{
    public NeuroPack(ReadOnlyTargetRules Target) : base(Target)
    {
        PCHUsage = PCHUsageMode.UseExplicitOrSharedPCHs;

        PublicDependencyModuleNames.AddRange(new string[]
        {
            "Core",
            "CoreUObject",
            "Engine",
        });

        // ── Locate pre-built neuropack shared library ──────────────────────
        // Expected layout (after `cargo build --release`):
        //   NeuroPack/Binaries/<Platform>/neuropack.dll   (Win64)
        //   NeuroPack/Binaries/<Platform>/libneuropack.so (Linux)
        //   NeuroPack/Binaries/<Platform>/libneuropack.dylib (Mac)

        string BinDir = Path.Combine(ModuleDirectory, "..", "..", "..", "Binaries",
                                     Target.Platform.ToString());

        if (Target.Platform == UnrealTargetPlatform.Win64)
        {
            string DllPath = Path.Combine(BinDir, "neuropack.dll");
            string LibPath = Path.Combine(BinDir, "neuropack.lib");

            if (File.Exists(LibPath))
            {
                PublicAdditionalLibraries.Add(LibPath);
                RuntimeDependencies.Add(DllPath);
                PublicDelayLoadDLLs.Add("neuropack.dll");
            }
            else
            {
                System.Console.WriteLine(
                    "NeuroPack: neuropack.lib not found at " + LibPath +
                    ". Run `cargo build --release` and copy outputs to Binaries/Win64/.");
            }
        }
        else if (Target.Platform == UnrealTargetPlatform.Linux)
        {
            string SoPath = Path.Combine(BinDir, "libneuropack.so");
            if (File.Exists(SoPath))
            {
                PublicAdditionalLibraries.Add(SoPath);
                RuntimeDependencies.Add(SoPath);
            }
        }
        else if (Target.Platform == UnrealTargetPlatform.Mac)
        {
            string DylibPath = Path.Combine(BinDir, "libneuropack.dylib");
            if (File.Exists(DylibPath))
            {
                PublicAdditionalLibraries.Add(DylibPath);
                RuntimeDependencies.Add(DylibPath);
            }
        }

        // Expose the NeuroPack C header to all consumers.
        PublicIncludePaths.Add(Path.Combine(ModuleDirectory, "Public"));
    }
}
