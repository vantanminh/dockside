//! Embedded Review, Merge Queue & Release Matrix Web UI served at `/`.

pub const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="vi">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>Dockside — Review &amp; Merge Queue</title>
<style>
  :root {
    --bg: #0f172a; --panel: #1e293b; --border: #334155; --text: #e2e8f0;
    --muted: #94a3b8; --accent: #38bdf8; --ok: #22c55e; --warn: #f59e0b; --err: #ef4444;
  }
  * { box-sizing: border-box; }
  body { margin: 0; font-family: ui-sans-serif, system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--text); }
  header { display: flex; flex-wrap: wrap; gap: 12px; align-items: center; justify-content: space-between; padding: 14px 20px; border-bottom: 1px solid var(--border); background: #0b1220; }
  header h1 { margin: 0; font-size: 18px; }
  .pill { display: inline-block; padding: 3px 9px; border-radius: 999px; font-size: 12px; font-weight: 600; background: #1e293b; border: 1px solid var(--border); }
  .pill.ok { color: #86efac; border-color: #166534; }
  .pill.warn { color: #fde68a; border-color: #92400e; }
  .pill.err { color: #fca5a5; border-color: #991b1b; }
  .pill.info { color: #7dd3fc; border-color: #075985; }
  main { display: grid; grid-template-columns: 380px 1fr; gap: 16px; padding: 16px 20px; max-width: 1500px; margin: 0 auto; }
  @media (max-width: 960px) { main { grid-template-columns: 1fr; } }
  .card { background: var(--panel); border: 1px solid var(--border); border-radius: 10px; padding: 14px; margin-bottom: 14px; }
  .card h2 { margin: 0 0 10px; font-size: 14px; text-transform: uppercase; letter-spacing: .04em; color: var(--muted); }
  .cs-item { padding: 10px; border: 1px solid var(--border); border-radius: 8px; margin-bottom: 8px; cursor: pointer; background: #162033; }
  .cs-item:hover, .cs-item.active { border-color: var(--accent); }
  .row { display: flex; justify-content: space-between; align-items: center; gap: 8px; }
  .muted { color: var(--muted); font-size: 12px; }
  button { cursor: pointer; border: 1px solid var(--border); background: #0f172a; color: var(--text); padding: 6px 12px; border-radius: 6px; font-size: 13px; font-weight: 600; }
  button.primary { background: #16a34a; border-color: #15803d; color: #fff; }
  button.danger { background: #dc2626; border-color: #b91c1c; color: #fff; }
  input { background: #0f172a; border: 1px solid var(--border); color: var(--text); padding: 6px 10px; border-radius: 6px; font-size: 13px; }
  pre.diff { background: #090f1d; border: 1px solid var(--border); border-radius: 8px; padding: 12px; overflow: auto; max-height: 520px; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 12px; line-height: 1.45; white-space: pre-wrap; }
  .add { color: #86efac; }
  .del { color: #fca5a5; }
  .hunk { color: #7dd3fc; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--border); }
  th { color: var(--muted); font-weight: 600; }
</style>
</head>
<body>
<header>
  <div class="row">
    <h1>Dockside</h1>
    <span id="meta" class="pill info">Loading...</span>
  </div>
  <div class="row">
    <input id="token" type="password" placeholder="Bearer token (nếu bật auth)" />
    <button onclick="refreshAll()">Tải lại</button>
  </div>
</header>
<main>
  <section>
    <div class="card">
      <h2>Changesets &amp; Merge Queue</h2>
      <div id="cs-list" class="muted">Đang tải...</div>
    </div>
    <div class="card">
      <h2>Active Path Leases</h2>
      <div id="leases" class="muted">Trống</div>
    </div>
  </section>
  <section>
    <div class="card" id="detail-card">
      <h2>Chi tiết Changeset &amp; Diff</h2>
      <div id="detail" class="muted">Chọn một changeset bên trái để xem diff, verify log và duyệt.</div>
    </div>
    <div class="card">
      <h2>Lịch sử Main (Revisions)</h2>
      <table>
        <thead><tr><th>Rev</th><th>CS</th><th>Author</th><th>Summary</th><th>Files</th></tr></thead>
        <tbody id="rev-body"></tbody>
      </table>
    </div>
    <div class="card">
      <h2>Deployments (Rudder / Workers / EAS)</h2>
      <table>
        <thead><tr><th>Target</th><th>Env</th><th>Rev</th><th>Artifact</th><th>Status</th></tr></thead>
        <tbody id="dep-body"></tbody>
      </table>
    </div>
  </section>
</main>
<script>
let selectedCs = null;

function authHeaders() {
  const t = document.getElementById('token').value.trim();
  const h = { 'Content-Type': 'application/json' };
  if (t) h['Authorization'] = 'Bearer ' + t;
  return h;
}

async function api(path, opts = {}) {
  const res = await fetch(path, { ...opts, headers: { ...authHeaders(), ...(opts.headers || {}) } });
  if (!res.ok) throw new Error(await res.text());
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : res.text();
}

function badgeClass(st) {
  if (st === 'landed') return 'ok';
  if (st === 'queued' || st === 'merging' || st === 'verifying') return 'warn';
  if (st === 'conflict' || st === 'failed') return 'err';
  return 'info';
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
}

function renderDiff(text) {
  if (!text.trim()) return '<span class="muted">(Không có thay đổi text)</span>';
  return text.split('\n').map(line => {
    if (line.startsWith('+') && !line.startsWith('+++')) return '<span class="add">' + esc(line) + '</span>';
    if (line.startsWith('-') && !line.startsWith('---')) return '<span class="del">' + esc(line) + '</span>';
    if (line.startsWith('@@')) return '<span class="hunk">' + esc(line) + '</span>';
    return esc(line);
  }).join('\n');
}

async function selectCs(csId) {
  selectedCs = csId;
  document.querySelectorAll('.cs-item').forEach(item => {
    item.classList.toggle('active', item.dataset.csId === selectedCs);
  });
  try {
    const [cs, diff] = await Promise.all([
      api('/v1/changesets/' + csId),
      api('/v1/changesets/' + csId + '/diff')
    ]);
    const triggers = (cs.sensitive_paths || []).length
      ? `<p class="muted">Đường dẫn nhạy cảm (tự kiểm tra nghiêm hơn, không cần duyệt): <strong>${esc(cs.sensitive_paths.join(', '))}</strong></p>` : '';
    const vr = cs.verify_report;
    const checks = vr
      ? `<p class="muted">Kiểm tra tự động: <strong>${vr.skipped ? 'bỏ qua — ' + esc(vr.skip_reason || '') : (vr.passed ? 'đạt' : 'không đạt')}</strong> (${(vr.steps || []).length} bước)</p>` : '';
    const err = cs.error_message ? `<p style="color:#fca5a5;">${esc(cs.error_message)}</p>` : '';
    const conflicts = (cs.conflicts || []).length
      ? `<p style="color:#fca5a5;">Conflicts (${cs.conflicts.length}): ${esc(cs.conflicts.map(c => c.path + ' (' + c.kind + ')').join(', '))}</p>` : '';
    document.getElementById('detail').innerHTML = `
      <div class="row">
        <strong>${esc(cs.cs_id)} — ${esc(cs.title)}</strong>
        <span class="pill ${badgeClass(cs.status)}">${esc(cs.status)}</span>
      </div>
      <p class="muted">Author: ${esc(cs.author)} | Base: r${cs.base_rev} ${cs.landed_rev ? '| Landed: r' + cs.landed_rev : ''} | Ops: ${cs.ops.length}</p>
      ${triggers}${checks}${err}${conflicts}
      <pre class="diff">${renderDiff(diff)}</pre>
    `;
  } catch (e) {
    document.getElementById('detail').textContent = 'Lỗi tải chi tiết: ' + e.message;
  }
}

async function refreshAll() {
  try {
    const health = await api('/v1/health');
    document.getElementById('meta').textContent =
      `${health.repo_name} | main: r${health.main_rev} | R2: ${health.r2_enabled ? 'ON' : 'local'} | Mirror: ${health.git_mirror_enabled ? 'ON' : 'OFF'}`;

    const [csList, leases, revs, deps] = await Promise.all([
      api('/v1/changesets?limit=50'),
      api('/v1/leases'),
      api('/v1/revisions?limit=25'),
      api('/v1/deployments?limit=20')
    ]);

    document.getElementById('cs-list').innerHTML = csList.length ? csList.map(c => `
      <div class="cs-item ${selectedCs === c.cs_id ? 'active' : ''}" data-cs-id="${esc(c.cs_id)}" onclick="selectCs('${esc(c.cs_id)}')">
        <div class="row"><strong>${esc(c.cs_id)}</strong><span class="pill ${badgeClass(c.status)}">${esc(c.status)}</span></div>
        <div style="margin:4px 0;font-size:13px;">${esc(c.title)}</div>
        <div class="muted">${esc(c.author)} · base r${c.base_rev} · ${c.ops.length} ops</div>
      </div>`).join('') : '<span class="muted">Chưa có changeset nào.</span>';

    document.getElementById('leases').innerHTML = leases.length ? leases.map(l =>
      `<div style="margin-bottom:6px;font-size:12px;"><code>${esc(l.path_prefix)}</code> — ${esc(l.cs_id)} (${esc(l.author)}, ${esc(l.mode)})</div>`
    ).join('') : '<span class="muted">Không có lease đang mở.</span>';

    document.getElementById('rev-body').innerHTML = revs.map(r => `
      <tr><td><strong>r${r.rev_id}</strong></td><td>${esc(r.cs_id || '-')}</td><td>${esc(r.author)}</td><td>${esc(r.summary)}</td><td>${r.touched_paths.length}</td></tr>
    `).join('');

    document.getElementById('dep-body').innerHTML = deps.length ? deps.map(d => `
      <tr><td>${esc(d.target)}</td><td>${esc(d.env)}</td><td>r${d.rev_id}</td><td><code>${esc(d.artifact_ref)}</code></td><td>${esc(d.status)}</td></tr>
    `).join('') : '<tr><td colspan="5" class="muted">Chưa có bản deploy nào.</td></tr>';
  } catch (e) {
    document.getElementById('meta').textContent = 'Lỗi kết nối: ' + e.message;
  }
}

refreshAll();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::DASHBOARD_HTML;

    #[test]
    fn changeset_selection_updates_highlight_before_loading_details() {
        let selection = DASHBOARD_HTML
            .split("async function selectCs(csId) {")
            .nth(1)
            .and_then(|script| script.split("  try {").next())
            .unwrap_or_default();
        assert!(selection.contains("selectedCs = csId;"));
        assert!(selection.contains("document.querySelectorAll('.cs-item').forEach(item => {"));
        assert!(
            selection
                .contains("item.classList.toggle('active', item.dataset.csId === selectedCs);")
        );
        assert!(DASHBOARD_HTML.contains("data-cs-id=\"${esc(c.cs_id)}\""));
        assert!(DASHBOARD_HTML.contains("selectedCs === c.cs_id ? 'active' : ''"));
    }
}
