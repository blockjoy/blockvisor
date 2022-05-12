<script lang="ts">
  import DataState from 'components/DataState/DataState.svelte';
  import { fadeDefault } from 'consts/animations';

  import Pill from 'components/Pill/Pill.svelte';

  import PillBox from 'components/PillBox/PillBox.svelte';

  import CopyNode from 'modules/dashboard/components/CopyNode/CopyNode.svelte';
  import Select from 'modules/forms/components/Select/Select.svelte';
  import TokenIcon from 'components/TokenIcon/TokenIcon.svelte';
  import ConfirmDeleteModal from 'modules/app/components/ConfirmDeleteModal/ConfirmDeleteModal.svelte';
  import { browser } from '$app/env';
  import Checkbox from 'modules/forms/components/Checkbox/Checkbox.svelte';
  import { useForm } from 'svelte-use-form';

  import IconDelete from 'icons/trash-12.svg';
  import IconUser from 'icons/person-12.svg';
  import IconButton from 'components/IconButton/IconButton.svelte';
  import { formatDistanceToNow } from 'date-fns';
  export let id;
  export let state = '';
  export let form;
  export let data: NodeDetails;

  const { ip_addr, address_name, status, created_at } = data;

  let isModalOpen = false;

  const handleDeleteModalOpen = (e) => {
    e.preventDefault();
    isModalOpen = true;
  };

  const handleDeleteModalClose = (e) => {
    isModalOpen = false;
  };

  const deleteForm = useForm({
    targetValue: {
      initial: 'DELETE',
    },
  });

  $: classes = ['details-header', `details-header--${state}`].join(' ');
</script>

<header class={classes}>
  <div>
    <h2 class="t-xlarge details-header__title">
      {address_name}

      <span class="details-header__icon">
        <TokenIcon icon="eth" />
      </span>
    </h2>
    <aside class="t-small details-header__summary">
      <div class="t-color-text-2 details-header__node-info">
        <CopyNode value={id}>
          <small
            title={id}
            class="t-color-text-3 t-ellipsis details-header__copytext"
            >{id}</small
          >
        </CopyNode>
        <small class="t-small">{ip_addr}</small>
        <date
          >{created_at && formatDistanceToNow(+new Date(created_at))} ago</date
        >
      </div>
      <PillBox>
        <Pill removable={false} transition={fadeDefault}>eth</Pill>
        <Pill removable={false} transition={fadeDefault}>validator</Pill>
        <Pill removable={false} transition={fadeDefault}>americas</Pill>
      </PillBox>
    </aside>
  </div>
  <form use:form class="details-header__controls">
    <Select
      labelClass="visually-hidden"
      items={[{ label: 'Group 1', value: 'g1' }]}
      value="g1"
      name="group"
      field={$form?.group}
      style="outline"
      size="medium"
      label="Select a date"
    >
      <svelte:fragment slot="label">Group</svelte:fragment>
    </Select>
    <div class="details-header__wrapper">
      <div class="t-uppercase t-microlabel details-header__state">
        <DataState {status} />
      </div>
      <div class="container--buttons">
        <IconButton style="outline" size="small">
          <IconUser />
        </IconButton>
        <IconButton
          on:click={handleDeleteModalOpen}
          style="outline"
          size="small"
        >
          <IconDelete />
        </IconButton>
      </div>
    </div>
  </form>
</header>

{#if browser && isModalOpen}
  <ConfirmDeleteModal
    id="delete-node-modal"
    form={deleteForm}
    on:submit={(e) => {
      console.log($deleteForm);
      e.preventDefault();
    }}
    {isModalOpen}
    handleModalClose={handleDeleteModalClose}
  >
    <svelte:fragment slot="label">
      Type “DELETE” to confirm deletion of “HostFox” host.
    </svelte:fragment>

    <div class="details-header__controls">
      <Checkbox field={$deleteForm.backup} name="backup"
        >Backup active node on host</Checkbox
      >
    </div>
  </ConfirmDeleteModal>
{/if}

<style>
  .details-header {
    margin-bottom: 40px;
    display: flex;
    flex-direction: column;
    gap: 28px;

    @media (--screen-medium) {
      margin-bottom: 80px;
      gap: 12px;
      justify-content: space-between;
      align-items: baseline;
      flex-direction: row;
    }

    &__controls {
      margin-top: 12px;
    }

    &__icon {
      display: inline-block;
      position: absolute;
      right: 0;
      top: 0;

      @media (--screen-medium-small) {
        left: -36px;
        right: auto;
      }
    }

    &__wrapper {
      display: flex;
      align-items: center;
      gap: 20px;
    }

    &__title {
      padding-right: 36px;
      position: relative;
      word-break: break-all;

      @media (--screen-medium-small) {
        padding-right: 0;
      }
    }

    &__summary {
      margin-top: 20px;
    }

    &__node-info {
      margin-bottom: 20px;
      display: flex;
      flex-wrap: wrap;
      gap: 8px 20px;
      align-items: center;
    }

    &__controls {
      display: flex;
      gap: 28px;
      flex-shrink: 0;
      flex-direction: column;

      @media (--screen-medium-small) {
        gap: 20px;
        align-items: center;
        flex-direction: row;
      }
    }

    &__icon {
      padding-top: 8px;
    }

    &__copytext {
      max-width: 80px;
    }

    &--consensus {
      & .details-header__icon {
        padding-top: 8px;
        color: theme(--color-primary);
      }

      & .details-header__state {
        color: theme(--color-primary);

        & :global(svg) {
          backface-visibility: hidden;
          animation: rotateClockwise 2s linear infinite reverse;
        }
      }
    }
  }
</style>
