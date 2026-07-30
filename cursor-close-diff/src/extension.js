const vscode = require('vscode');
const path = require('path');
const os = require('os');
const fs = require('fs');
const net = require('net');

let watcher = null;
let server = null;
let statusBar = null;
let provider = null;
let enabled = true;

// ─── Path helpers ───────────────────────────────────────────────────────

function getGrokHome() {
    const config = vscode.workspace.getConfiguration('abxgliaDiffBridge');
    return config.get('grokHome') || process.env.GROK_HOME || path.join(os.homedir(), '.abxglia-grok');
}

function getInstancesDir() {
    return path.join(getGrokHome(), 'instances');
}

function getSocketPath() {
    return path.join(getGrokHome(), `sock-${process.pid}`);
}

function getInstanceFile() {
    return path.join(getInstancesDir(), `${process.pid}.json`);
}

function getWorkspaceFolder() {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders || folders.length === 0) return '';
    return folders[0].uri.fsPath;
}

// ─── Virtual document provider (serves diff content from memory) ─────────

const contentMap = new Map(); // uri.toString() → text content
let providerRef = null;       // EventEmitter for change notifications

/**
 * Register a TextDocumentContentProvider for the `grokdiff:` scheme. This lets
 * us open diffs with custom URIs (and thus custom labels) without writing temp
 * files. The provider serves old/new text from the in-memory `contentMap`.
 *
 * A monotonically increasing token is embedded in each URI so that repeated
 * edits to the SAME file get distinct URIs — VS Code caches virtual documents
 * per-URI, so without the token, a second edit to the same file would show the
 * stale first-edit content.
 */
function registerDiffProvider(context) {
    const emitter = new vscode.EventEmitter();
    providerRef = emitter;
    provider = {
        onDidChange: emitter.event,
        provideTextDocumentContent(uri) {
            return contentMap.get(uri.toString()) ?? '';
        },
    };
    provider.setContent = function(uri, text) {
        contentMap.set(uri.toString(), text);
        emitter.fire(uri);
    };
    const registration = vscode.workspace.registerTextDocumentContentProvider(
        'grokdiff',
        provider
    );
    context.subscriptions.push(registration);
}

// Monotonic counter for unique diff URIs (avoids VS Code's virtual-doc cache).
let diffToken = 0;

// ─── Diff opening via vscode.diff (custom title + per-side labels) ────────

/**
 * Open a diff in this window using vscode.diff with virtual document URIs.
 * The tab title shows the real file path, and each side shows the real
 * basename — no temp files, no ugly .tmpXXX names.
 *
 * Each call uses a unique token in the URI so repeated edits to the same file
 * are treated as fresh documents (VS Code caches virtual docs per-URI).
 */
function openDiff(filePath, oldText, newText) {
    const label = filePath.split('/').pop().split('\\').pop(); // basename
    const token = ++diffToken;
    const oldUri = vscode.Uri.parse(`grokdiff:/old/${token}/${encodeURIComponent(filePath)}`);
    const newUri = vscode.Uri.parse(`grokdiff:/new/${token}/${encodeURIComponent(filePath)}`);

    provider.setContent(oldUri, oldText);
    provider.setContent(newUri, newText);

    // The 3rd arg is the tab title. Show the basename + "(grok review)".
    vscode.commands.executeCommand(
        'vscode.diff',
        oldUri,
        newUri,
        `${label} (grok review)`,
        { preview: false, viewColumn: vscode.ViewColumn.Active }
    ).then(
        () => vscode.window.setStatusBarMessage(`Abxglia: diff opened for ${label}`, 3000),
        (err) => vscode.window.setStatusBarMessage(`Abxglia: diff failed — ${err}`, 5000)
    );
}

// ─── Socket server (receives diff requests from grok) ────────────────────

function setupSocketServer(context) {
    const sockPath = getSocketPath();

    // Clean up any stale socket file.
    try { fs.unlinkSync(sockPath); } catch (_) { /* didn't exist */ }

    server = net.createServer((socket) => {
        let data = '';
        socket.on('data', (chunk) => { data += chunk.toString(); });

        socket.on('end', () => {
            try {
                const req = JSON.parse(data);
                if (req.action === 'close') {
                    // Close command — close the active diff tab immediately.
                    // This is the race-free replacement for the file-based signal.
                    closeActiveDiffTab();
                    socket.end(JSON.stringify({ ok: true }));
                } else if (req.filePath && 'oldText' in req && 'newText' in req) {
                    openDiff(req.filePath, req.oldText, req.newText);
                    socket.end(JSON.stringify({ ok: true }));
                } else {
                    socket.end(JSON.stringify({ ok: false, error: 'missing fields' }));
                }
            } catch (e) {
                socket.end(JSON.stringify({ ok: false, error: String(e) }));
            }
        });

        socket.on('error', () => { /* client disconnected; ignore */ });
    });

    server.listen(sockPath, () => {
        console.log(`Abxglia diff bridge listening on ${sockPath}`);
    });

    server.on('error', (err) => {
        console.error('Abxglia diff bridge socket error:', err);
    });

    context.subscriptions.push({
        dispose: () => {
            try { server.close(); } catch (_) {}
            try { fs.unlinkSync(sockPath); } catch (_) {}
        }
    });
}

// ─── Instance registry ───────────────────────────────────────────────────

function registerInstance() {
    const dir = getInstancesDir();
    fs.mkdirSync(dir, { recursive: true });

    const descriptor = {
        pid: process.pid,
        socketPath: getSocketPath(),
        workspaceFolder: getWorkspaceFolder(),
        startedAt: Date.now(),
    };

    fs.writeFileSync(getInstanceFile(), JSON.stringify(descriptor, null, 2));
}

function unregisterInstance() {
    try { fs.unlinkSync(getInstanceFile()); } catch (_) { /* already gone */ }
    try { fs.unlinkSync(getSocketPath()); } catch (_) { /* already gone */ }
}

// ─── Close-signal watcher (existing, kept as-is) ─────────────────────────

function closeActiveDiffTab() {
    if (!enabled) return;

    const activeTab = vscode.window.tabGroups.activeTabGroup.activeTab;
    if (!activeTab) return;

    vscode.window.tabGroups.close(activeTab).then(
        () => vscode.window.setStatusBarMessage('Abxglia: diff tab closed', 3000),
        () => vscode.commands.executeCommand('workbench.action.closeActiveEditor')
    );
}

function setupCloseSignalWatcher(context) {
    const signalPath = path.join(getGrokHome(), '.close-diff-signal');
    const pattern = new vscode.RelativePattern(
        vscode.Uri.file(path.dirname(signalPath)),
        path.basename(signalPath)
    );
    watcher = vscode.workspace.createFileSystemWatcher(pattern);
    watcher.onDidChange(closeActiveDiffTab);
    watcher.onDidCreate(closeActiveDiffTab);
    context.subscriptions.push({ dispose: () => watcher && watcher.dispose() });
}

// ─── Status bar ──────────────────────────────────────────────────────────

function setupStatusBar(context) {
    const folder = getWorkspaceFolder();
    const label = folder ? path.basename(folder) : 'no workspace';
    statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
    statusBar.text = `$(diff) grok: ${label}`;
    statusBar.tooltip = `Abxglia diff bridge — workspace: ${folder || '(none)'}`;
    statusBar.show();
    context.subscriptions.push(statusBar);
}

// ─── Activation / deactivation ───────────────────────────────────────────

function activate(context) {
    const config = vscode.workspace.getConfiguration('abxgliaDiffBridge');
    enabled = config.get('enabled', true);

    registerDiffProvider(context);
    setupSocketServer(context);
    registerInstance();
    setupCloseSignalWatcher(context);
    setupStatusBar(context);

    // Toggle command.
    context.subscriptions.push(
        vscode.commands.registerCommand('abxglia-diff-bridge.toggle', () => {
            enabled = !enabled;
            vscode.window.showInformationMessage(`Abxglia Diff Bridge: ${enabled ? 'enabled' : 'disabled'}`);
        })
    );

    // React to config changes.
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration('abxgliaDiffBridge')) {
                const newConfig = vscode.workspace.getConfiguration('abxgliaDiffBridge');
                enabled = newConfig.get('enabled', true);
            }
        })
    );

    // Clean up instance descriptor when the window closes.
    context.subscriptions.push({
        dispose: () => unregisterInstance()
    });

    console.log('Abxglia Diff Bridge extension activated');
}

function deactivate() {
    unregisterInstance();
}

module.exports = { activate, deactivate };
