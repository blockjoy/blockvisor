<script lang="ts">
  import Sorter from 'components/Sorter/Sorter.svelte';
  import HostDataRow from './HostDataRow.svelte';

  export let hosts;

  let sortActive;

  const handleSort = (id: string, value: SorterValues) => {
    sortActive = { id, value };
  };
</script>
<table class="table hosts-table">
  <colgroup>
    <col width="320px" />
    <col width="100px" />
    <col />
  </colgroup>
  <thead>
    <tr>
      <th class="table__heading">
        <Sorter callback={handleSort} active={sortActive} id="name">Name</Sorter
        >
      </th>
      <th class="table__heading">
        <Sorter callback={handleSort} active={sortActive} id="status"
          >Status</Sorter
        >
      </th>
      <th class="table__heading">
        <span class="visually-hidden">Actions</span>
      </th>
    </tr>
  </thead>
  <tbody>
    
    {#each hosts as host}
    <HostDataRow {...host} />
    {/each}
  </tbody>
</table>

<style>
  .hosts-table {
    @media (--screen-medium-larger-max) {
      thead {
        display: none;
      }

      &,
      & :global(tbody) {
        display: block;
        width: 100%;
      }
    }
  }
</style>
