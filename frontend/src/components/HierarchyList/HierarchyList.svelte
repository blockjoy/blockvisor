<script lang="ts">
  import type { HierarchyListProps } from './HierarchyList.svelte';

  import Node from 'components/HierarchyList/Node.svelte';

  export let nodes: HierarchyListProps['nodes'] = [];
  export let handleConfirm;
  export let handleEdit;
  export let editingId;
  export let hideList;
</script>

{#if Boolean(nodes.length)}
  <ul class="u-list-reset hierarchy-list">
    {#each nodes as { children, ...node }}
      <Node on:click {...node} isParent={Boolean(children?.length)}>
        {#if !hideList}
          {#if Boolean(children?.length) || $$slots.action}
            <ul class="u-list-reset hierarchy-list__list">
              {#each children as child}
                <Node
                  isEditing={editingId === child.id}
                  {handleConfirm}
                  {handleEdit}
                  on:click
                  {...child}
                  isParent={false}
                />
              {/each}
              {#if $$slots.default}
                <slot />
              {/if}
              {#if $$slots.action}
                <li class="hierarchy-list__action">
                  <slot name="action" />
                </li>
              {/if}
            </ul>
          {/if}
        {/if}
      </Node>
    {/each}
  </ul>
{/if}

<style>
  .hierarchy-list {
    padding-bottom: 32px;
    position: sticky;
    top: 0;
    left: 0;
    max-height: calc(100vh - 65px);
    overflow-y: auto;

    @media (--screen-medium-large) {
      padding-top: 32px;
    }

    & :global(.hierarchy-list__list) {
      padding-left: 8px;
    }

    & :global(li + li) {
      margin-top: 4px;
    }

    &__list {
      margin-top: 4px;
    }
  }
</style>
