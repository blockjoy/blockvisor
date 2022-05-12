<script lang="ts">
import { format } from 'date-fns';

  import HostDataRow from './HostDataRow.svelte';


  export let data : HostDetails;

  const {
  os,
  os_version,
  created_at} = data;
</script>

<table class="table details-table">
  <colgroup>
    <col width="180px" />
    <col />
  </colgroup>

  <tbody>
    <HostDataRow>
      <svelte:fragment slot="title">OS</svelte:fragment>
      {os}
    </HostDataRow>
    <HostDataRow>
      <svelte:fragment slot="title">OS Version</svelte:fragment>
      {os_version}
    </HostDataRow>
    <HostDataRow>
      <svelte:fragment slot="title">Created</svelte:fragment>
      {created_at && format(+new Date(created_at),'MM/dd/yyyy')}
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
