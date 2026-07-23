<script lang="ts">
  import { project } from "../stores";
</script>

<header class="top">
  <span class="label">project</span>
  <span class="name">{$project?.project_name ?? ""}</span>
  <span class="path">{$project?.project_dir ?? ""}</span>
</header>

<style>
  .top {
    grid-area: top;
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.35rem 0.75rem;
    background: var(--bg-elev);
    border-bottom: 1px solid var(--border);
    white-space: nowrap;
    overflow: hidden;
  }
  .label {
    color: var(--label);
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }
  .name {
    font-weight: 600;
  }
  .path {
    color: var(--text-faint);
    font-size: 0.78rem;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
