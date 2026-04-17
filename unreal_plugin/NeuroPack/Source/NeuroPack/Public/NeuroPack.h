#pragma once

// NeuroPack.h — Unreal Engine C++ wrapper for the NeuroPack C FFI.
//
// Usage:
//   1. Call UNeuropackLibrary::OpenPackage(FilePath) once at startup.
//   2. Call ReadAsset(Handle, RelPath) to decompress any asset into a TArray.
//   3. Call ClosePackage(Handle) when done (e.g. level unload).
//
// Thread safety: Each FNeuropackHandle is NOT thread-safe.
// Create one handle per thread, or guard with a critical section.

#include "CoreMinimal.h"
#include "Kismet/BlueprintFunctionLibrary.h"
#include "NeuroPack.generated.h"

// ── Opaque handle ──────────────────────────────────────────────────────────

USTRUCT(BlueprintType)
struct NEUROPACK_API FNeuropackHandle
{
    GENERATED_BODY()

    FNeuropackHandle() : Ptr(nullptr) {}
    explicit FNeuropackHandle(void* InPtr) : Ptr(InPtr) {}

    FORCEINLINE bool IsValid() const { return Ptr != nullptr; }

private:
    void* Ptr;
    friend class UNeuropackLibrary;
};

// ── Blueprint function library ─────────────────────────────────────────────

UCLASS()
class NEUROPACK_API UNeuropackLibrary : public UBlueprintFunctionLibrary
{
    GENERATED_BODY()

public:
    /**
     * Open a .neuropack package file for reading.
     * @param FilePath  Absolute or project-relative path to the package.
     * @return          Opaque handle; invalid on failure.  Check IsValid().
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static FNeuropackHandle OpenPackage(const FString& FilePath);

    /**
     * Close a handle returned by OpenPackage.  Safe to call with invalid handle.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static void ClosePackage(FNeuropackHandle Handle);

    /**
     * Decompress a single asset from an open package into OutBytes.
     * @param Handle    Valid handle from OpenPackage.
     * @param RelPath   Relative path inside the package, e.g. "textures/hero.dds".
     * @param OutBytes  Receives the raw decompressed asset bytes.
     * @return          true on success, false on failure.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static bool ReadAsset(FNeuropackHandle Handle,
                          const FString& RelPath,
                          TArray<uint8>& OutBytes);

    /**
     * Return the number of entries in an open package's index.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static int32 GetEntryCount(FNeuropackHandle Handle);

    /**
     * Return the relative path of entry at index I.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static FString GetEntryPath(FNeuropackHandle Handle, int32 Index);

    /**
     * Return the uncompressed byte size of entry at index I.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static int64 GetEntrySize(FNeuropackHandle Handle, int32 Index);

    /**
     * Return the last error string from the C FFI layer.
     */
    UFUNCTION(BlueprintCallable, Category = "NeuroPack")
    static FString GetLastError();
};
