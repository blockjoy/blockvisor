<script lang="ts">
  import DataState from 'components/DataState/DataState.svelte';

  import DropdownLinkList from 'components/Dropdown/DropdownList.svelte';
  import DropdownItem from 'components/Dropdown/DropdownItem.svelte';
  import { ROUTES } from 'consts/routes';
  import IconAccount from 'icons/person-12.svg';
  import IconDocument from 'icons/document-12.svg';
  import IconDots from 'icons/dots-12.svg';
  import ButtonWithDropdown from 'modules/app/components/ButtonWithDropdown/ButtonWithDropdown.svelte';
  import CopyNode from 'modules/dashboard/components/CopyNode/CopyNode.svelte';
  import TokenIcon from 'components/TokenIcon/TokenIcon.svelte';
  import { formatDistanceToNow } from 'date-fns';

  export let name = '';
  export let status = '';
  export let ipAddr = '';
  export let id = '';
  export let createdAt = '';
  export let linkToHostDetails;

  const classes = ['table__row host-data-row', `host-data-row--${status}`].join(
    ' ',
  );
</script>

<tr class={classes}>
  <td class="host-data-row__token">
    <TokenIcon icon={'eth'} />
  </td>

  <td class="host-data-row__col host-data-row__details">
    <div title={name} class="t-ellipsis">
      <a
        class="u-link-reset host-data-row__link"
        href={linkToHostDetails
          ? ROUTES.HOST_DETAILS(id)
          : ROUTES.NODE_DETAILS(id)}
      >
        {name}
      </a>
    </div>
    <small class="t-small t-color-text-2 host-data-row__details-added"
      >{createdAt && formatDistanceToNow(+new Date(createdAt))}</small
    >
    <div class="node-data-row__info">
      <CopyNode value="abc">
        <small
          title={`Copy ${id} to clipboard`}
          class="t-small t-color-text-3 host-data-row__text t-ellipsis"
          >{id}</small
        >
      </CopyNode>
      <span class="t-small t-color-text-2">{ipAddr}</span>
    </div>
  </td>
  <td class="t-small t-color-text-2 host-data-row__col host-data-row__added"
    >{createdAt && formatDistanceToNow(+new Date(createdAt))} ago</td
  >
  <td class="t-uppercase t-color-text-3 t-microlabel host-data-row__state">
    <DataState {status} />
  </td>
  <td class="host-data-row__action">
    <ButtonWithDropdown
      position="right"
      iconButton
      buttonProps={{ style: 'ghost', size: 'tiny' }}
      slot="action"
    >
      <svelte:fragment slot="label">
        <IconDots />
      </svelte:fragment>
      <DropdownLinkList slot="content">
        <li>
          <DropdownItem as="button" href="#">
            <IconAccount />
            Profile</DropdownItem
          >
        </li>
        <li>
          <DropdownItem as="button" href="#">
            <IconDocument />
            Billing</DropdownItem
          >
        </li>
      </DropdownLinkList>
    </ButtonWithDropdown>
  </td>
</tr>

<style>
  .host-data-row {
    @media (--screen-medium-larger-max) {
      padding-top: 32px;
      padding-bottom: 18px;
      display: block;
      width: 100%;
      position: relative;
    }

    & :global(.dropdown) {
      margin-top: 0;
    }

    & :global(td) {
      line-height: 20px;
      vertical-align: top;
      padding-top: 24px;
      padding-bottom: 12px;

      @media (--screen-medium-larger-max) {
        padding: 0;
        display: block;
        padding-left: 36px;
      }
    }

    @media (--screen-medium-larger-max) {
      & > &__details {
        padding-right: clamp(100px, 40%, 200px);
      }

      &__added {
        display: none;
      }
    }

    &__col {
      padding-right: 28px;
      vertical-align: top;
    }

    &__token {
      color: theme(--color-text-2);
      & :global(svg) {
        display: inline-block;
      }

      @media (--screen-medium-larger-max) {
        position: absolute;
        left: -36px;
        text-align: left;
        top: 32px;
      }
    }

    &__action {
      text-align: right;
      margin-top: -20px;
    }

    &__text {
      max-width: 90px;
    }

    @media (--screen-medium-larger-max) {
      &__state {
        position: absolute;
        top: 32px;
        right: 0;
      }
    }

    &__link-icon {
      display: inline-block;
      padding: 0 12px;
    }

    &__details {
      &-added {
        display: none;

        @media (--screen-medium-larger-max) {
          display: block;
          margin-top: 4px;
          margin-bottom: 16px;
        }
      }
    }

    &__info {
      margin-top: 8px;
      display: flex;
      gap: 12px;

      @media (--screen-medium-larger-max) {
        flex-wrap: wrap;
      }
    }

    &--consensus {
      border-bottom: 0;
      background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 512 1'%3E%3Cpath stroke='url(%23a)' d='M0 .5h512'/%3E%3Cdefs%3E%3ClinearGradient id='a' x1='512' x2='0' y1='1' y2='1' gradientUnits='userSpaceOnUse'%3E%3Cstop stop-color='%23BFF589' stop-opacity='0'/%3E%3Cstop offset='.5' stop-color='%23BFF589'/%3E%3Cstop offset='1' stop-color='%23BFF589' stop-opacity='0'/%3E%3C/linearGradient%3E%3C/defs%3E%3C/svg%3E");
      border-width: 0;
      background-position-y: 100%;
      background-repeat: no-repeat;

      & .host-data-row__details,
      & .host-data-row__token,
      & .host-data-row__state {
        color: var(--color-primary);
      }

      & .host-data-row__state {
        & :global(svg) {
          backface-visibility: hidden;
          animation: rotateClockwise 2s linear infinite reverse;
        }
      }
    }
  }
</style>
