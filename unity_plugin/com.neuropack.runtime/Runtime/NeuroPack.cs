// NeuroPack.cs — Unity P/Invoke bindings for the NeuroPack native library.
//
// Setup:
//   1. Copy neuropack.dll (Win), libneuropack.so (Linux), or
//      libneuropack.dylib (Mac) into this package's Plugins/ folder so that
//      Unity's native plugin importer picks it up automatically.
//   2. Use NeuropackReader in any MonoBehaviour or ScriptableObject.
//
// Thread safety: NeuropackReader is NOT thread-safe.  If you need parallel
// reads, create one reader per thread (each backed by its own native handle).
//
// Memory: ReadAsset copies bytes into a managed byte[]; the native buffer is
// freed automatically after the copy.

using System;
using System.Runtime.InteropServices;
using UnityEngine;

namespace NeuroPack
{
    // ── P/Invoke declarations ──────────────────────────────────────────────

    internal static class Native
    {
        const string Lib = "neuropack";

        [DllImport(Lib, EntryPoint = "neuropack_last_error",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr LastError();

        [DllImport(Lib, EntryPoint = "neuropack_open",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern IntPtr Open([MarshalAs(UnmanagedType.LPStr)] string path);

        [DllImport(Lib, EntryPoint = "neuropack_close",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern void Close(IntPtr handle);

        [DllImport(Lib, EntryPoint = "neuropack_read_asset",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern int ReadAsset(IntPtr handle,
                                           [MarshalAs(UnmanagedType.LPStr)] string relPath,
                                           out IntPtr outData,
                                           out UIntPtr outLen);

        [DllImport(Lib, EntryPoint = "neuropack_free_asset",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern void FreeAsset(IntPtr data, UIntPtr len);

        [DllImport(Lib, EntryPoint = "neuropack_entry_count",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern UIntPtr EntryCount(IntPtr handle);

        [DllImport(Lib, EntryPoint = "neuropack_entry_path",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern UIntPtr EntryPath(IntPtr handle, UIntPtr index,
                                               IntPtr buf, UIntPtr bufLen);

        [DllImport(Lib, EntryPoint = "neuropack_entry_size",
                   CallingConvention = CallingConvention.Cdecl)]
        public static extern ulong EntrySize(IntPtr handle, UIntPtr index);

        /// <summary>Marshal the last error C string to a managed string.</summary>
        public static string GetLastError()
        {
            IntPtr ptr = LastError();
            return ptr == IntPtr.Zero ? "(no error)" : Marshal.PtrToStringAnsi(ptr) ?? "";
        }
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /// <summary>
    /// Opens a .neuropack file and exposes its contents for reading.
    /// Implements IDisposable; use in a <c>using</c> block or call
    /// <see cref="Dispose"/> when done.
    /// </summary>
    public sealed class NeuropackReader : IDisposable
    {
        private IntPtr _handle;

        /// <summary>Open a package at <paramref name="path"/>.</summary>
        /// <exception cref="Exception">Thrown when the native open call fails.</exception>
        public NeuropackReader(string path)
        {
            _handle = Native.Open(path);
            if (_handle == IntPtr.Zero)
                throw new Exception($"NeuroPack: failed to open '{path}': {Native.GetLastError()}");
        }

        /// <summary>Number of entries in the package index.</summary>
        public int EntryCount => (int)Native.EntryCount(_handle);

        /// <summary>Relative path of entry at <paramref name="index"/>.</summary>
        public string GetEntryPath(int index)
        {
            const int BufSize = 1024;
            IntPtr buf = Marshal.AllocHGlobal(BufSize);
            try
            {
                Native.EntryPath(_handle, (UIntPtr)index, buf, (UIntPtr)BufSize);
                return Marshal.PtrToStringAnsi(buf) ?? string.Empty;
            }
            finally { Marshal.FreeHGlobal(buf); }
        }

        /// <summary>Uncompressed byte size of entry at <paramref name="index"/>.</summary>
        public long GetEntrySize(int index) =>
            (long)Native.EntrySize(_handle, (UIntPtr)index);

        /// <summary>
        /// Decompress a single asset by its relative path.
        /// </summary>
        /// <param name="relPath">e.g. <c>"textures/hero.dds"</c></param>
        /// <returns>Raw decompressed bytes.</returns>
        /// <exception cref="Exception">Thrown when the asset is not found or decompression fails.</exception>
        public byte[] ReadAsset(string relPath)
        {
            int rc = Native.ReadAsset(_handle, relPath, out IntPtr data, out UIntPtr len);
            if (rc != 0)
                throw new Exception($"NeuroPack: ReadAsset('{relPath}') failed: {Native.GetLastError()}");

            int length = (int)(ulong)len;
            byte[] result = new byte[length];
            Marshal.Copy(data, result, 0, length);
            Native.FreeAsset(data, len);
            return result;
        }

        /// <summary>
        /// Try to decompress an asset; returns false and sets
        /// <paramref name="error"/> on failure instead of throwing.
        /// </summary>
        public bool TryReadAsset(string relPath, out byte[] bytes, out string error)
        {
            int rc = Native.ReadAsset(_handle, relPath, out IntPtr data, out UIntPtr len);
            if (rc != 0)
            {
                bytes = Array.Empty<byte>();
                error = Native.GetLastError();
                return false;
            }

            int length = (int)(ulong)len;
            bytes = new byte[length];
            Marshal.Copy(data, bytes, 0, length);
            Native.FreeAsset(data, len);
            error = null;
            return true;
        }

        /// <summary>
        /// Load a Texture2D directly from a DDS/PNG/JPEG asset in the package.
        /// </summary>
        public Texture2D LoadTexture(string relPath, bool mipChain = false)
        {
            byte[] data = ReadAsset(relPath);
            var tex = new Texture2D(2, 2, TextureFormat.RGBA32, mipChain);
            tex.name = System.IO.Path.GetFileNameWithoutExtension(relPath);
            if (!ImageConversion.LoadImage(tex, data))
                Debug.LogWarning($"NeuroPack: LoadImage failed for '{relPath}'");
            return tex;
        }

        /// <summary>
        /// Load an AudioClip from a WAV asset in the package (synchronous).
        /// For streaming, use <see cref="ReadAsset"/> and feed bytes to
        /// your audio decoder directly.
        /// </summary>
        public AudioClip LoadAudioClip(string relPath)
        {
            byte[] data = ReadAsset(relPath);
            // Unity does not provide a synchronous API for non-WAV formats.
            // For OGG/MP3 use a third-party decoder or load from disk after extraction.
            return WavUtility.ToAudioClip(data,
                System.IO.Path.GetFileNameWithoutExtension(relPath));
        }

        // ── IDisposable ────────────────────────────────────────────────────

        public void Dispose()
        {
            if (_handle != IntPtr.Zero)
            {
                Native.Close(_handle);
                _handle = IntPtr.Zero;
            }
        }

        ~NeuropackReader() => Dispose();
    }

    // ── Minimal WAV decoder ────────────────────────────────────────────────
    // Only covers PCM WAV (16-bit stereo/mono).  Replace with a real WAV
    // library (e.g. NAudio) for production use.

    internal static class WavUtility
    {
        public static AudioClip ToAudioClip(byte[] data, string name)
        {
            const int HeaderSize = 44;
            if (data.Length < HeaderSize)
                return AudioClip.Create(name, 0, 1, 44100, false);

            int channels    = BitConverter.ToInt16(data, 22);
            int sampleRate  = BitConverter.ToInt32(data, 24);
            int bitDepth    = BitConverter.ToInt16(data, 34);
            int dataLen     = BitConverter.ToInt32(data, 40);
            int sampleCount = dataLen / (bitDepth / 8);

            float[] samples = new float[sampleCount];
            if (bitDepth == 16)
            {
                for (int i = 0; i < sampleCount; i++)
                    samples[i] = BitConverter.ToInt16(data, HeaderSize + i * 2) / 32768f;
            }

            var clip = AudioClip.Create(name, sampleCount / channels, channels, sampleRate, false);
            clip.SetData(samples, 0);
            return clip;
        }
    }
}
