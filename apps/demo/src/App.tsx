import { useState, useEffect } from "react";
import { save, open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import {
  useCollection,
  useDbStats,
  useNetworkStatus,
  useDbPath,
  type Record,
} from "@xdb/react";
import "./App.css";

// Demo-specific types
interface Note {
  title: string;
  content: string;
  color: string;
}

interface Task {
  title: string;
  completed: boolean;
  priority: "low" | "medium" | "high";
}

interface Contact {
  name: string;
  email: string;
  phone: string;
}

type DemoTab = "notes" | "tasks" | "contacts";

function App() {
  const [activeTab, setActiveTab] = useState<DemoTab>("notes");
  const [notification, setNotification] = useState<string | null>(null);

  // Show notification
  const showNotification = (message: string) => {
    setNotification(message);
    setTimeout(() => setNotification(null), 3000);
  };

  // Listen for sync events
  useEffect(() => {
    const unlistenSync = listen("xdb-sync-event", (event) => {
      showNotification(`Synced: ${JSON.stringify(event.payload)}`);
    });

    const unlistenPeer = listen("xdb-peer-event", (event: { payload: { type: string; peer_id: string } }) => {
      const { type, peer_id } = event.payload;
      showNotification(
        `Peer ${type}: ${peer_id.substring(0, 16)}...`
      );
    });

    const unlistenImport = listen("db-imported", () => {
      showNotification("Database imported! Refreshing...");
      window.location.reload();
    });

    return () => {
      unlistenSync.then((f) => f());
      unlistenPeer.then((f) => f());
      unlistenImport.then((f) => f());
    };
  }, []);

  return (
    <div className="app">
      <header className="header">
        <h1>XDB Demo</h1>
        <p>Local-First P2P Database</p>
      </header>

      {notification && <div className="notification">{notification}</div>}

      <div className="main-content">
        <Sidebar activeTab={activeTab} setActiveTab={setActiveTab} />
        <div className="content">
          {activeTab === "notes" && <NotesPanel />}
          {activeTab === "tasks" && <TasksPanel />}
          {activeTab === "contacts" && <ContactsPanel />}
        </div>
        <StatusPanel showNotification={showNotification} />
      </div>
    </div>
  );
}

// Sidebar Component
function Sidebar({
  activeTab,
  setActiveTab,
}: {
  activeTab: DemoTab;
  setActiveTab: (tab: DemoTab) => void;
}) {
  return (
    <nav className="sidebar">
      <button
        className={activeTab === "notes" ? "active" : ""}
        onClick={() => setActiveTab("notes")}
      >
        Notes
      </button>
      <button
        className={activeTab === "tasks" ? "active" : ""}
        onClick={() => setActiveTab("tasks")}
      >
        Tasks
      </button>
      <button
        className={activeTab === "contacts" ? "active" : ""}
        onClick={() => setActiveTab("contacts")}
      >
        Contacts
      </button>
    </nav>
  );
}

// Notes Panel
function NotesPanel() {
  const { records, loading, create, update, remove, requestSync } =
    useCollection<Note>("notes");
  const [newNote, setNewNote] = useState({ title: "", content: "", color: "#ffeb3b" });
  const [editingId, setEditingId] = useState<string | null>(null);

  const handleCreate = async () => {
    if (!newNote.title.trim()) return;
    await create(newNote);
    setNewNote({ title: "", content: "", color: "#ffeb3b" });
  };

  const handleUpdate = async (id: string, data: Note) => {
    await update(id, data);
    setEditingId(null);
  };

  const colors = ["#ffeb3b", "#ff9800", "#4caf50", "#2196f3", "#e91e63", "#9c27b0"];

  return (
    <div className="panel">
      <div className="panel-header">
        <h2>Notes</h2>
        <button className="sync-btn" onClick={requestSync}>
          Sync
        </button>
      </div>

      <div className="create-form">
        <input
          type="text"
          placeholder="Note title..."
          value={newNote.title}
          onChange={(e) => setNewNote({ ...newNote, title: e.target.value })}
        />
        <textarea
          placeholder="Note content..."
          value={newNote.content}
          onChange={(e) => setNewNote({ ...newNote, content: e.target.value })}
        />
        <div className="color-picker">
          {colors.map((color) => (
            <button
              key={color}
              className={`color-btn ${newNote.color === color ? "selected" : ""}`}
              style={{ backgroundColor: color }}
              onClick={() => setNewNote({ ...newNote, color })}
            />
          ))}
        </div>
        <button className="create-btn" onClick={handleCreate}>
          Add Note
        </button>
      </div>

      {loading ? (
        <p>Loading...</p>
      ) : (
        <div className="notes-grid">
          {records.map((record) => {
            const note = record.data as Note;
            return (
              <div
                key={record.id}
                className="note-card"
                style={{ backgroundColor: note.color }}
              >
                {editingId === record.id ? (
                  <NoteEditor
                    note={note}
                    onSave={(data) => handleUpdate(record.id, data)}
                    onCancel={() => setEditingId(null)}
                  />
                ) : (
                  <>
                    <h3>{note.title}</h3>
                    <p>{note.content}</p>
                    <div className="card-actions">
                      <button onClick={() => setEditingId(record.id)}>Edit</button>
                      <button onClick={() => remove(record.id)}>Delete</button>
                    </div>
                    <small>{new Date(record.created_at).toLocaleString()}</small>
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function NoteEditor({
  note,
  onSave,
  onCancel,
}: {
  note: Note;
  onSave: (note: Note) => void;
  onCancel: () => void;
}) {
  const [edited, setEdited] = useState(note);

  return (
    <div className="editor">
      <input
        type="text"
        value={edited.title}
        onChange={(e) => setEdited({ ...edited, title: e.target.value })}
      />
      <textarea
        value={edited.content}
        onChange={(e) => setEdited({ ...edited, content: e.target.value })}
      />
      <div className="editor-actions">
        <button onClick={() => onSave(edited)}>Save</button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </div>
  );
}

// Tasks Panel
function TasksPanel() {
  const { records, loading, create, update, remove, requestSync } =
    useCollection<Task>("tasks");
  const [newTask, setNewTask] = useState<Task>({ title: "", completed: false, priority: "medium" });

  const handleCreate = async () => {
    if (!newTask.title.trim()) return;
    await create(newTask);
    setNewTask({ title: "", completed: false, priority: "medium" });
  };

  const toggleComplete = async (record: Record) => {
    const task = record.data as Task;
    await update(record.id, { ...task, completed: !task.completed });
  };

  const getPriorityColor = (priority: string) => {
    switch (priority) {
      case "high":
        return "#f44336";
      case "medium":
        return "#ff9800";
      case "low":
        return "#4caf50";
      default:
        return "#9e9e9e";
    }
  };

  return (
    <div className="panel">
      <div className="panel-header">
        <h2>Tasks</h2>
        <button className="sync-btn" onClick={requestSync}>
          Sync
        </button>
      </div>

      <div className="create-form row-form">
        <input
          type="text"
          placeholder="Task title..."
          value={newTask.title}
          onChange={(e) => setNewTask({ ...newTask, title: e.target.value })}
        />
        <select
          value={newTask.priority}
          onChange={(e) =>
            setNewTask({ ...newTask, priority: e.target.value as Task["priority"] })
          }
        >
          <option value="low">Low</option>
          <option value="medium">Medium</option>
          <option value="high">High</option>
        </select>
        <button className="create-btn" onClick={handleCreate}>
          Add Task
        </button>
      </div>

      {loading ? (
        <p>Loading...</p>
      ) : (
        <div className="task-list">
          {records.map((record) => {
            const task = record.data as Task;
            return (
              <div
                key={record.id}
                className={`task-item ${task.completed ? "completed" : ""}`}
              >
                <input
                  type="checkbox"
                  checked={task.completed}
                  onChange={() => toggleComplete(record)}
                />
                <span
                  className="priority-dot"
                  style={{ backgroundColor: getPriorityColor(task.priority) }}
                />
                <span className="task-title">{task.title}</span>
                <button className="delete-btn" onClick={() => remove(record.id)}>
                  X
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

// Contacts Panel
function ContactsPanel() {
  const { records, loading, create, update, remove, requestSync } =
    useCollection<Contact>("contacts");
  const [newContact, setNewContact] = useState({ name: "", email: "", phone: "" });
  const [editingId, setEditingId] = useState<string | null>(null);

  const handleCreate = async () => {
    if (!newContact.name.trim()) return;
    await create(newContact);
    setNewContact({ name: "", email: "", phone: "" });
  };

  const handleUpdate = async (id: string, data: Contact) => {
    await update(id, data);
    setEditingId(null);
  };

  return (
    <div className="panel">
      <div className="panel-header">
        <h2>Contacts</h2>
        <button className="sync-btn" onClick={requestSync}>
          Sync
        </button>
      </div>

      <div className="create-form">
        <input
          type="text"
          placeholder="Name..."
          value={newContact.name}
          onChange={(e) => setNewContact({ ...newContact, name: e.target.value })}
        />
        <input
          type="email"
          placeholder="Email..."
          value={newContact.email}
          onChange={(e) => setNewContact({ ...newContact, email: e.target.value })}
        />
        <input
          type="tel"
          placeholder="Phone..."
          value={newContact.phone}
          onChange={(e) => setNewContact({ ...newContact, phone: e.target.value })}
        />
        <button className="create-btn" onClick={handleCreate}>
          Add Contact
        </button>
      </div>

      {loading ? (
        <p>Loading...</p>
      ) : (
        <div className="contact-list">
          {records.map((record) => {
            const contact = record.data as Contact;
            return (
              <div key={record.id} className="contact-card">
                {editingId === record.id ? (
                  <ContactEditor
                    contact={contact}
                    onSave={(data) => handleUpdate(record.id, data)}
                    onCancel={() => setEditingId(null)}
                  />
                ) : (
                  <>
                    <div className="contact-info">
                      <h3>{contact.name}</h3>
                      <p>{contact.email}</p>
                      <p>{contact.phone}</p>
                    </div>
                    <div className="card-actions">
                      <button onClick={() => setEditingId(record.id)}>Edit</button>
                      <button onClick={() => remove(record.id)}>Delete</button>
                    </div>
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function ContactEditor({
  contact,
  onSave,
  onCancel,
}: {
  contact: Contact;
  onSave: (contact: Contact) => void;
  onCancel: () => void;
}) {
  const [edited, setEdited] = useState(contact);

  return (
    <div className="editor">
      <input
        type="text"
        value={edited.name}
        onChange={(e) => setEdited({ ...edited, name: e.target.value })}
        placeholder="Name"
      />
      <input
        type="email"
        value={edited.email}
        onChange={(e) => setEdited({ ...edited, email: e.target.value })}
        placeholder="Email"
      />
      <input
        type="tel"
        value={edited.phone}
        onChange={(e) => setEdited({ ...edited, phone: e.target.value })}
        placeholder="Phone"
      />
      <div className="editor-actions">
        <button onClick={() => onSave(edited)}>Save</button>
        <button onClick={onCancel}>Cancel</button>
      </div>
    </div>
  );
}

// Status Panel
function StatusPanel({ showNotification }: { showNotification: (msg: string) => void }) {
  const { stats } = useDbStats();
  const { status } = useNetworkStatus();
  const dbPath = useDbPath();

  const handleExport = async () => {
    const path = await save({
      filters: [{ name: "SQLite Database", extensions: ["sqlite", "db"] }],
      defaultPath: "xdb-backup.sqlite",
    });
    if (path) {
      try {
        await invoke("export_database", { path });
        showNotification(`Database exported to ${path}`);
      } catch (e) {
        showNotification(`Export failed: ${e}`);
      }
    }
  };

  const handleImport = async () => {
    const path = await open({
      filters: [{ name: "SQLite Database", extensions: ["sqlite", "db"] }],
      multiple: false,
    });
    if (path) {
      try {
        await invoke("import_database", { sourcePath: path });
        showNotification("Database imported successfully!");
      } catch (e) {
        showNotification(`Import failed: ${e}`);
      }
    }
  };

  const formatBytes = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  };

  return (
    <aside className="status-panel">
      <h3>Network Status</h3>
      <div className="status-item">
        <span>Status:</span>
        <span className={status?.is_running ? "status-online" : "status-offline"}>
          {status?.is_running ? "Online" : "Offline"}
        </span>
      </div>
      <div className="status-item">
        <span>Peer ID:</span>
        <span className="peer-id">{status?.peer_id?.substring(0, 16) || "-"}...</span>
      </div>
      <div className="status-item">
        <span>Connected Peers:</span>
        <span>{status?.connected_peers?.length || 0}</span>
      </div>

      {status?.connected_peers && status.connected_peers.length > 0 && (
        <div className="peer-list">
          <h4>Connected Peers</h4>
          {status.connected_peers.map((peer) => (
            <div key={peer} className="peer-item">
              {peer.substring(0, 20)}...
            </div>
          ))}
        </div>
      )}

      <h3>Database</h3>
      <div className="status-item">
        <span>Records:</span>
        <span>{stats?.record_count || 0}</span>
      </div>
      <div className="status-item">
        <span>Collections:</span>
        <span>{stats?.collection_count || 0}</span>
      </div>
      <div className="status-item">
        <span>Size:</span>
        <span>{stats ? formatBytes(stats.db_size_bytes) : "-"}</span>
      </div>
      <div className="status-item db-path">
        <span>Path:</span>
        <span title={dbPath}>{dbPath.split(/[/\\]/).pop() || "-"}</span>
      </div>

      <h3>Backup / Restore</h3>
      <div className="backup-buttons">
        <button onClick={handleExport}>Export Database</button>
        <button onClick={handleImport}>Import Database</button>
      </div>
    </aside>
  );
}

export default App;
