<script lang="ts">
  import { format } from 'date-fns';

  import HostDataRow from './HostDataRow.svelte';

  export let data: HostDetails;

  const { os, os_version, created_at, cpu_count, disk_size, mem_size } = data;
</script>

<table class="table details-table">
  <colgroup>
    <col width="180px" />
    <col />
  </colgroup>

  <tbody>
    <HostDataRow>
      <svelte:fragment slot="title">OS</svelte:fragment>
      {os || 'n/a'}
    </HostDataRow>

    <HostDataRow>
      <svelte:fragment slot="title">OS Version</svelte:fragment>
      {os_version || 'n/a'}
    </HostDataRow>

    <HostDataRow>
      <svelte:fragment slot="title">CPU CORES</svelte:fragment>
      {cpu_count || 'n/a'}
    </HostDataRow>

    <HostDataRow>
      <svelte:fragment slot="title">MEMORY</svelte:fragment>
      {mem_size ? `${(mem_size / 1000000).toFixed(2)} GB` : 'n/a'}
    </HostDataRow>

    <HostDataRow>
      <svelte:fragment slot="title">DISK SIZE</svelte:fragment>
      {disk_size ? `${(disk_size / 1000000).toFixed(2)} GB` : 'n/a'}
    </HostDataRow>

    <HostDataRow>
      <svelte:fragment slot="title">Created</svelte:fragment>
      {created_at ? format(+new Date(created_at), 'MM/dd/yyyy') : 'n/a'}
    </HostDataRow>
  </tbody>
</table>

<style>
  .details-table {
    margin-bottom: 40px;

    @media (--screen-smaller-max) {
      display: block;

      & tbody {
        display: block;
      }
    }
  }
</style>
