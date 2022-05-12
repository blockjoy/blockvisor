<script lang="ts">
  import HierarchyList from 'components/HierarchyList/HierarchyList.svelte';
  import GroupEdit from 'modules/forms/components/GroupEdit/GroupEdit.svelte';
  import { fetchAllHosts, hosts } from 'modules/hosts/store/hostsStore';
  import NodeGroupAdd from 'modules/nodes/components/NodeGroupAdd/NodeGroupAdd.svelte';
  import { onMount } from 'svelte';

  let isAddingNewGroup = false;
  let editingId;

  const handleEdit = (e) => (editingId = e.target.value);
  const handleEditConfirm = () => (editingId = null);
  const handleAddConfirm = () => (isAddingNewGroup = false);
  const handleAddGroup = () => (isAddingNewGroup = true);

  onMount(() => {
    fetchAllHosts();
  });
</script>

<HierarchyList
  on:click
  handleConfirm={handleEditConfirm}
  {handleEdit}
  hideList={true}
  {editingId}
  nodes={$hosts}
>
  {#if isAddingNewGroup}
    <li>
      <GroupEdit handleConfirm={handleAddConfirm} />
    </li>
  {/if}

  <NodeGroupAdd
    disabled={isAddingNewGroup}
    on:click={handleAddGroup}
    slot="action"
  />
</HierarchyList>
