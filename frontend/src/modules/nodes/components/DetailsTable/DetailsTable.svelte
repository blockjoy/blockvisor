<script lang="ts">
  import DataRow from 'modules/nodes/components/DetailsTable/DataRow.svelte';

  export let data: NodeDetails;

  const {
    owner_address,
    version,
    block_height,
    tenure_penalty,
    performance_penalty,
    dkg_penalty,
    total_penalty,
    user_id,
  } = data;
</script>

<table class="table">
  <colspan>
    <col width="80px" />
    <col />
  </colspan>
  <tbody>
    <DataRow>
      <svelte:fragment slot="label">Owner address</svelte:fragment>
      {owner_address}
    </DataRow>
    <DataRow>
      <svelte:fragment slot="label">Version</svelte:fragment>
      {version}
    </DataRow>
    <DataRow>
      <svelte:fragment slot="label">Block height</svelte:fragment>
      {block_height}
    </DataRow>
    <DataRow>
      <svelte:fragment slot="label">Penalties</svelte:fragment>
      {`Tenure: ${tenure_penalty}, DKG: ${dkg_penalty}, Perf: ${performance_penalty}, Tot: ${total_penalty}`}
    </DataRow>
    <DataRow>
      <svelte:fragment slot="label">User</svelte:fragment>
      {user_id}
    </DataRow>
  </tbody>
</table>
