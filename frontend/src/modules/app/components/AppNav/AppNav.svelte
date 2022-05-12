<script lang="ts">
  import LogoBlockVisor from 'logos/app-blockvisor.svg';
  import LogoBroadcast from 'logos/app-broadcast.svg';
  import { APPS } from 'models/App';
  import { app } from 'modules/app/store';
  import AppButton from './AppButton.svelte';

  export let callback;

  $: activeApp = $app.activeApp;

  const handleAppChange = (e) => {
    const { value = APPS.BLOCKVISOR } = e.currentTarget;

    const newValue = parseInt(value) as APPS;

    if (isNaN(newValue) || newValue === activeApp) {
      return;
    }

    e.currentTarget?.blur();

    app.setActiveApp(newValue);
    callback?.(newValue);
  };
</script>

<nav class="app-nav">
  <ul class="u-list-reset app-nav__list">
    <li class="app-nav__item">
      <AppButton {activeApp} on:click={handleAppChange} value={APPS.BLOCKVISOR}>
        <LogoBlockVisor />
        <span class="app-nav__label t-micro">BlockVisor</span>
      </AppButton>
    </li>
    <li class="app-nav__item">
      <AppButton {activeApp} on:click={handleAppChange} value={APPS.BROADCAST}>
        <LogoBroadcast />
        <span class="app-nav__label  t-micro">Broadcast</span>
      </AppButton>
    </li>
  </ul>
  <div class="app-nav__pointer" />
</nav>

<style>
  .app-nav {
    padding: 16px 12px;
    border-right: 1px solid theme(--color-text-5-o10);

    &__item {
      position: relative;
    }

    &__label {
      display: none;
      position: absolute;
      color: theme(--color-text-4);
      background-color: theme(--color-overlay-background-1);
      padding: 4px 8px;
      border-radius: 4px;
      left: 100%;
      top: 50%;
      transform: translate3d(0, -50%, 0);
      margin-left: 4px;
      pointer-events: none;
      z-index: var(--level-1);
    }

    &__list {
      display: flex;
      gap: 12px;
      flex-direction: column;
      align-items: center;
    }
  }
</style>
