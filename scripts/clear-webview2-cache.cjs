// Clears the WebView2 persistent cache for WUPI so a freshly-built exe
// doesn't serve stale frontend from the previous run. Run before every build.
//
// AUTO-CLOSES a running WUPI first: if wupi.exe is alive it holds a lock on
// the EBWebView cache dir, and rmSync would fail with EPERM. Rather than make
// the user remember to close the app, this script force-kills wupi.exe (and
// its WebView2 child processes) before clearing. Safe no-op when nothing is
// running. The kill is Windows-only + guarded so non-Windows CI is unaffected.
const { rmSync, existsSync } = require('fs');
const { join } = require('path');
const { platform } = require('os');

const cacheDir = join(
  process.env.LOCALAPPDATA || join(require('os').homedir(), 'AppData', 'Local'),
  'com.wupi.desktop',
  'EBWebView'
);

// Kill any process that could hold a lock on the cache dir. Uses taskkill
// (Windows only). The /T flag takes down child processes too (the WebView2
// renderer/network/utility processes wupi.exe spawns). /F = force (the app
// gets no chance to prompt "save before quit", which is intentional for a
// build step — the user explicitly ran a release). Errors (process not found,
// not on Windows) are swallowed: the clear below is the authoritative step.
function killRunningWupi() {
  if (platform() !== 'win32') return;
  const { execFileSync } = require('child_process');
  // msedgewebview2.exe children can outlive a graceful wupi.exe exit and keep
  // the cache locked; kill both. taskkill prints "SUCCESS" or "ERROR: not
  // found" to stderr — we discard both (we only care that they're gone now).
  for (const exe of ['wupi.exe', 'msedgewebview2.exe']) {
    try {
      execFileSync('taskkill', ['/F', '/T', '/IM', exe], { stdio: 'ignore' });
      console.log(`[clear-webview2-cache] closed running ${exe}`);
    } catch {
      // Not running (most common) or taskkill unavailable — either way,
      // nothing to kill. Proceed to the cache clear.
    }
  }
}

killRunningWupi();

if (existsSync(cacheDir)) {
  try {
    rmSync(cacheDir, { recursive: true, force: true });
    console.log('[clear-webview2-cache] cleared:', cacheDir);
  } catch (err) {
    // Even after the kill, the lock can take a moment to release as the OS
    // tears down the process. A short retry covers that race. If it STILL
    // fails, surface a clear message instead of a raw stack trace.
    const isLockErr = err.code === 'EPERM' || err.code === 'EBUSY' || err.code === 'ENOTEMPTY';
    if (!isLockErr) throw err; // genuine FS error (disk full, permissions) — rethrow
    require('child_process').execSync(
      `powershell -NoProfile -Command "Start-Sleep -Milliseconds 400"`,
      { stdio: 'ignore' }
    );
    try {
      rmSync(cacheDir, { recursive: true, force: true });
      console.log('[clear-webview2-cache] cleared (after retry):', cacheDir);
    } catch (err2) {
      console.error('[clear-webview2-cache] could not clear cache — close WUPI and retry.');
      console.error('  ' + cacheDir);
      console.error('  (' + err2.code + ': ' + err2.message.split('\n')[0] + ')');
      process.exit(1);
    }
  }
} else {
  console.log('[clear-webview2-cache] no cache to clear');
}
