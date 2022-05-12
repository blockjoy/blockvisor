<script lang="ts">
    export let value;
    export let label;
    export let maxValue
</script>

<div class="host-container">
    <slot />
    <div class="host-info">
        <span class="host-info__label">{label}</span>
        <span class="host-info__value">{value}</span>
        <span class="host-info__limit">/ {maxValue}</span>
    </div>
</div>

<style>
    .host-container {
        display: flex;
        align-items: center;
        margin-bottom: 60px;
        

        @media (min-width: 768px) {
            margin-left: 70px;
        }
    }
    .host-info {
        display: flex;
        flex-direction: column;
        align-items: center;
        max-width: 100px;
        margin-left: 24px;

        &__label {
            font-size: 10px;
            line-height: 16px;
            margin-bottom: 8px;
            letter-spacing: 1px;
            color: #A5A8A3;
            text-transform: uppercase;
            align-self: baseline;
        }

        &__limit {
            font-size: 12px;
            line-height: 16px;
            color: #5F615D;
            text-transform: uppercase;
            align-self: baseline;
        }

        &__value {
            font-size: 24px;
            line-height: 32px;
            color: #85F550;
            text-transform: uppercase;
            align-self: baseline;
        }
    }
</style>