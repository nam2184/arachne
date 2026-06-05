import { useEditorStore } from "./editorStore";

export function EditorPane() {
  const { tabs, activeTabId, setActiveTab, closeTab } = useEditorStore();

  return (
    <div className="editor-pane">
      <div className="file-tabs">
        {tabs.map((tab) => (
          <div
            key={tab.id}
            className={`file-tab ${tab.id === activeTabId ? "active" : ""}`}
            onClick={() => setActiveTab(tab.id)}
          >
            <span>{tab.name}</span>
            <button onClick={(e) => { e.stopPropagation(); closeTab(tab.id); }}>×</button>
          </div>
        ))}
      </div>
      <div className="monaco-container" />
    </div>
  );
}