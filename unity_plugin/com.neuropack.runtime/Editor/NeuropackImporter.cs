// NeuropackImporter.cs — Unity Editor custom asset importer for .neuropack files.
//
// Registers .neuropack as a recognized asset type in the Project window.
// Double-clicking opens a read-only inspector listing all entries with
// their sizes and compression ratios.

#if UNITY_EDITOR
using System.IO;
using UnityEditor;
using UnityEditor.AssetImporters;
using UnityEngine;

namespace NeuroPack.Editor
{
    [ScriptedImporter(version: 1, ext: "neuropack")]
    public class NeuropackImporter : ScriptedImporter
    {
        public override void OnImportAsset(AssetImportContext ctx)
        {
            // We represent the .neuropack as a TextAsset so the Project window
            // shows a file icon.  The real data is read at runtime via NeuropackReader.
            var textAsset = new TextAsset($"NeuroPack package: {Path.GetFileName(ctx.assetPath)}");
            ctx.AddObjectToAsset("main", textAsset);
            ctx.SetMainObject(textAsset);
        }
    }

    [CustomEditor(typeof(TextAsset))]
    public class NeuropackInspector : UnityEditor.Editor
    {
        private string _packagePath;
        private Vector2 _scroll;
        private string[] _paths;
        private long[]   _sizes;
        private float[]  _ratios;
        private bool     _loaded;
        private string   _error;

        private void OnEnable()
        {
            _packagePath = AssetDatabase.GetAssetPath(target);
            if (!_packagePath.EndsWith(".neuropack", System.StringComparison.OrdinalIgnoreCase))
                return;

            _packagePath = Path.GetFullPath(_packagePath);
            LoadIndex();
        }

        private void LoadIndex()
        {
            try
            {
                using var reader = new NeuropackReader(_packagePath);
                int count = reader.EntryCount;
                _paths  = new string[count];
                _sizes  = new long[count];
                _ratios = new float[count];
                for (int i = 0; i < count; i++)
                {
                    _paths[i]  = reader.GetEntryPath(i);
                    _sizes[i]  = reader.GetEntrySize(i);
                }
                _loaded = true;
            }
            catch (System.Exception ex)
            {
                _error = ex.Message;
            }
        }

        public override void OnInspectorGUI()
        {
            if (!_packagePath.EndsWith(".neuropack", System.StringComparison.OrdinalIgnoreCase))
            {
                base.OnInspectorGUI();
                return;
            }

            EditorGUILayout.LabelField("NeuroPack Package", EditorStyles.boldLabel);
            EditorGUILayout.LabelField(Path.GetFileName(_packagePath));
            EditorGUILayout.Space();

            if (_error != null)
            {
                EditorGUILayout.HelpBox(_error, MessageType.Error);
                if (GUILayout.Button("Retry")) { _error = null; LoadIndex(); }
                return;
            }

            if (!_loaded) { EditorGUILayout.LabelField("Loading…"); return; }

            EditorGUILayout.LabelField($"{_paths.Length} entries", EditorStyles.miniLabel);
            EditorGUILayout.Space();

            _scroll = EditorGUILayout.BeginScrollView(_scroll, GUILayout.Height(300));
            for (int i = 0; i < _paths.Length; i++)
            {
                EditorGUILayout.BeginHorizontal();
                EditorGUILayout.LabelField(_paths[i], GUILayout.ExpandWidth(true));
                EditorGUILayout.LabelField(
                    FormatBytes(_sizes[i]),
                    EditorStyles.miniLabel,
                    GUILayout.Width(80));
                EditorGUILayout.EndHorizontal();
            }
            EditorGUILayout.EndScrollView();
        }

        private static string FormatBytes(long b)
        {
            if (b < 1024)        return $"{b} B";
            if (b < 1024 * 1024) return $"{b / 1024.0:F1} KB";
            return $"{b / 1048576.0:F1} MB";
        }
    }
}
#endif
