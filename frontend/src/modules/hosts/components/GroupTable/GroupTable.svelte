<script lang="ts">
  import Sorter from 'components/Sorter/Sorter.svelte';
  import HostDataRow from './HostDataRow.svelte';

  export let hosts;

  let sortActive;

  const handleSort = (id: string, value: SorterValues) => {
    sortActive = { id, value };
  };
  console.log("hosts", hosts)
</script>
<table class="table hosts-table">
  <colgroup>
    <col width="40px" />
    <col width="280px" />
    <col width="160px" />
    <col />
    <col />
  </colgroup>
  <thead>
    <tr>
      <th class="table__heading">
        <Sorter id="token" active={sortActive} callback={handleSort}>
          <span class="visually-hidden">Token</span>
        </Sorter>
      </th>
      <th class="table__heading">
        <Sorter id="name" active={sortActive} callback={handleSort}>Name</Sorter
        >
      </th>
      <th class="table__heading">
        <Sorter id="added" active={sortActive} callback={handleSort}
          >Added</Sorter
        >
      </th>
      <th class="table__heading">
        <Sorter id="status" active={sortActive} callback={handleSort}
          >Status</Sorter
        >
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
    margin-top: 60px;

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
