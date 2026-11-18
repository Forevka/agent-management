<script lang="ts">
  import { onMount } from "svelte";
  import { open as pickFolder } from "@tauri-apps/plugin-dialog";
  import {
    getState,
    mapProject,
    message,
    openPath,
    unmapProject,
    type TrayState,
  } from "./api";

  const REFRESH_MS = 5000;

  let tray = $state<TrayState | null>(null);
  // Owned by refresh(): set and cleared on every load.
  let loadError = $state("");
  // Owned by the list actions: cleared only by the next action or by dismissing it, so a
  // refresh never wipes an error before it has been read.
  let actionError = $state("");
  // The map form's own action error, same rule.
  let formError = $state("");
  let busy = $state(false);
  let confirming = $state<string | null>(null);
  let slug = $state("");
  let folder = $state("");

  const status = $derived(tray?.status ?? null);
  const stale = $derived(tray?.status_stale ?? false);
  // One rule for the dropdown and the "not on orchestrator" badge: only a non-empty list from
  // a running node counts. A stale or empty list could flag every mapping for no reason.
  const projectList = $derived(
    status && !stale && status.projects && status.projects.length > 0 ? status.projects : null,
  );
  const known = $derived(projectList ? new Set(projectList.map((p) => p.slug)) : null);
  const existing = $derived(tray?.mappings.find((m) => m.slug === slug.trim()) ?? null);

  async function refresh() {
    try {
      tray = await getState();
      loadError = "";
    } catch (e) {
      // Keep showing the last good state; the error explains why it may be out of date.
      loadError = message(e);
    }
  }

  async function run(action: () => Promise<void>, setError: (e: string) => void) {
    busy = true;
    setError("");
    try {
      await action();
    } catch (e) {
      setError(message(e));
    } finally {
      busy = false;
      await refresh();
    }
  }

  async function choose() {
    formError = "";
    try {
      const picked = await pickFolder({ directory: true, multiple: false, title: "Project folder" });
      if (picked) folder = picked;
    } catch (e) {
      formError = message(e);
    }
  }

  function map(event: SubmitEvent) {
    event.preventDefault();
    const s = slug.trim();
    return run(
      async () => {
        await mapProject(s, folder);
        slug = "";
        folder = "";
      },
      (e) => (formError = e),
    );
  }

  function unmap(s: string) {
    confirming = null;
    return run(() => unmapProject(s), (e) => (actionError = e));
  }

  function openFolder(path: string) {
    return run(() => openPath(path), (e) => (actionError = e));
  }

  onMount(() => {
    refresh();
    const onFocus = () => refresh();
    window.addEventListener("focus", onFocus);
    // The window is hidden, never closed. The Rust side hides the webview with it, which is
    // what turns visibilityState to "hidden" in WebView2, so polling stops while out of sight.
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") refresh();
    }, REFRESH_MS);
    return () => {
      window.removeEventListener("focus", onFocus);
      clearInterval(timer);
    };
  });
</script>

<main>
  <header>
    {#if status && stale}
      <div class="status"><span class="dot unknown"></span> Node not running</div>
      {#if status.updated_at}
        <div class="muted small">last status {status.updated_at}</div>
      {/if}
    {:else if status}
      <div class="status">
        <span class="dot" class:on={status.connected}></span>
        {#if status.connected}
          Connected{status.node_name ? ` as ${status.node_name}` : ""}
        {:else}
          Disconnected{status.node_name ? ` (${status.node_name})` : ""}
        {/if}
      </div>
      {#if status.updated_at}
        <div class="muted small">updated {status.updated_at}</div>
      {/if}
    {:else}
      <div class="status"><span class="dot unknown"></span> Status unknown</div>
    {/if}
    {#if tray}
      <div class="muted small path" title={tray.data_dir}>Data dir: {tray.data_dir}</div>
    {/if}
  </header>

  <section>
    <h2>Mapped projects</h2>
    {#if loadError}
      <p class="error" role="alert">{loadError}</p>
    {/if}
    {#if actionError}
      <p class="error dismissable" role="alert">
        <span>{actionError}</span>
        <button class="dismiss" aria-label="Dismiss" onclick={() => (actionError = "")}>×</button>
      </p>
    {/if}
    {#if tray && tray.mappings.length === 0}
      <p class="muted">No projects are mapped on this node yet.</p>
    {:else if tray}
      <ul>
        {#each tray.mappings as m (m.slug)}
          <li>
            <div class="info">
              <div>
                <strong>{m.slug}</strong>
                {#if known && !known.has(m.slug)}
                  <span class="badge" title="The orchestrator has no project with this slug">
                    not on orchestrator
                  </span>
                {/if}
              </div>
              <div class="muted small path" title={m.path}>{m.path}</div>
            </div>
            <div class="actions">
              {#if confirming === m.slug}
                <span class="small">Unmap {m.slug}?</span>
                <button class="danger" disabled={busy} onclick={() => unmap(m.slug)}>Unmap</button>
                <button onclick={() => (confirming = null)}>Cancel</button>
              {:else}
                <button disabled={busy} onclick={() => openFolder(m.path)}>Open folder</button>
                <button disabled={busy} onclick={() => (confirming = m.slug)}>Unmap</button>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section>
    <h2>Map a project</h2>
    <form onsubmit={map}>
      <label>
        <span>Project</span>
        {#if projectList}
          <select bind:value={slug}>
            <option value="" disabled>Choose a project</option>
            {#each projectList as p (p.slug)}
              <option value={p.slug}>{p.name ? `${p.name} (${p.slug})` : p.slug}</option>
            {/each}
          </select>
        {:else}
          <input
            bind:value={slug}
            placeholder="project-slug"
            autocomplete="off"
            spellcheck="false"
          />
          <span class="muted small">
            The orchestrator's project list is not available, so type the project's slug.
          </span>
        {/if}
      </label>

      <label>
        <span>Folder</span>
        <div class="row">
          <input readonly value={folder} placeholder="No folder chosen" title={folder} />
          <button type="button" onclick={choose}>Choose…</button>
        </div>
      </label>

      {#if existing}
        <p class="note">
          {existing.slug} is already mapped to {existing.path}. Mapping it again replaces that folder.
        </p>
      {/if}
      {#if formError}
        <p class="error dismissable" role="alert">
          <span>{formError}</span>
          <button type="button" class="dismiss" aria-label="Dismiss" onclick={() => (formError = "")}>×</button>
        </p>
      {/if}

      <div>
        <button class="primary" type="submit" disabled={busy || !slug.trim() || !folder}>Map</button>
      </div>
    </form>
  </section>
</main>
