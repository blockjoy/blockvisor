<script lang="ts">
  import DataState from 'components/DataState/DataState.svelte';

  import DropdownLinkList from 'components/Dropdown/DropdownList.svelte';
  import DropdownItem from 'components/Dropdown/DropdownItem.svelte';

  import IconAccount from 'icons/person-12.svg';
  import IconDocument from 'icons/document-12.svg';
  import IconDots from 'icons/dots-12.svg';
  import ButtonWithDropdown from 'modules/app/components/ButtonWithDropdown/ButtonWithDropdown.svelte';
import CopyNode from 'modules/dashboard/components/CopyNode/CopyNode.svelte';
import TokenIcon from 'components/TokenIcon/TokenIcon.svelte';

  export let name;
  export let status;
  export let ip_addr;
  export let url;
  export let location;
  export let id;

  const classes = ['table__row host-data-row', `host-data-row--${status}`].join(
    ' ',
  );
</script>

<tr class={classes}>
  <td class="node-data-row__token">
    <TokenIcon icon={"algo"} />
  </td>

  <td class="host-data-row__col host-data-row__details">
    <a class="u-link-reset" href={url}>
      {name}
    </a>
    <div class="host-data-row__info">
      <CopyNode value="abc">
        <small
          title={`Copy ${id} to clipboard`}
          >{id}</small
        >
      </CopyNode>
      <span class="t-color-text-2">{ip_addr}</span>
    </div>
  </td>
  <td class="host-data-row__col t-uppercase t-microlabel host-data-row__state">
    <!-- Temp fix since statuses are not the same and we don't have icons for new statuses. -->
    <DataState {status} />
  </td>
  <td class="host-data-row__col t-right">
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
    position: relative;

    & :global(.dropdown) {
      margin-top: 0;
    }

    &--issue {
      color: theme(--color-utility-warning);

      & :global(.data-state) {
        color: theme(--color-utility-warning);

        & :global(svg) {
          animation: blink 1s var(--transition-easing-cubic) infinite alternate;
        }
      }
    }

    @media (--screen-medium-larger-max) {
      padding-top: 32px;
      padding-bottom: 18px;
      display: block;
      width: 100%;
      position: relative;
    }

    &__col {
      padding: 24px 0 12px;
      vertical-align: top;

      @media (--screen-medium-larger-max) {
        padding: 0;
        display: block;
      }
    }

    &__info {
      display: flex;
      flex-wrap: wrap;
      gap: 12px;
      color: theme(--color-text-2);
      padding-top: 8px;
    }

    &__state {
      padding-top: 28px;
      color: theme(--color-text-3);
    }

    &__link {
      display: inline-block;
      padding: 6px 12px;

      @media (--screen-medium-larger-max) {
        position: absolute;
        bottom: 16px;
        right: 0;
      }
    }
  }
</style>
