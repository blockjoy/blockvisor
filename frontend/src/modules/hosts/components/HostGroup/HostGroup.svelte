<script lang="ts">
  import GroupTitle from 'components/GroupTitle/GroupTitle.svelte';
  import IconBox from 'icons/box-12.svg';
  import EmptyColumn from 'modules/dashboard/components/EmptyStates/EmptyColumn.svelte';
  import GroupTable from 'modules/hosts/components/GroupTable/GroupTable.svelte';

  export let selectedHosts;
  export let id;
</script>

<article class="host-group">
  <div class="host-group__section">
    <GroupTitle>
      <slot name="title" slot="title" />
      <slot name="action" slot="action" />
      <svelte:fragment slot="stats">
        <IconBox />
        {selectedHosts?.validators?.length || 0} hosts
      </svelte:fragment>
    </GroupTitle>
  </div>

  {#if selectedHosts.validators?.length}
    <GroupTable hosts={selectedHosts.validators} />
  {:else}
    <div class="host-group--empty">
      <EmptyColumn id={`graphic-${id}`}>
        <svelte:fragment slot="title">No Hosts In Group</svelte:fragment>
        <svelte:fragment slot="description"
          >Assign a host to this group to easily track multiple host
          performance.</svelte:fragment
        >
      </EmptyColumn>
    </div>
  {/if}
</article>

<style>
  .host-group {
    margin-bottom: 40px;

    &--empty {
      max-width: 484px;
    }

    &__section {
      margin-bottom: 40px;
    }

    @media (--screen-medium) {
      margin-bottom: 120px;
    }
  }
</style>
