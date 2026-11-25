# Profile-2 bundled-control CDC and reset-release constraints.
#
# The 160-bit payload is captured in the 80 MHz control domain, held for one
# complete source cycle before request assertion, and kept stable through the
# four-phase response. Bound the complete payload path to that 12.5 ns prepare
# interval. Its destination capture is handshake-qualified rather than
# edge-related, so ordinary asynchronous-clock hold analysis does not apply.

set profile2_payload_sources [get_registers {
    *rf_profile2_control_cdc:control_bridge|request_words_control*
    *rf_profile2_control_cdc:control_bridge|request_epoch_control*
}]
set profile2_payload_destinations [get_registers {
    *rf_profile2_control_cdc:control_bridge|armed_words_rx*
    *rf_profile2_control_cdc:control_bridge|armed_epoch_rx*
}]

if {
    [get_collection_size $profile2_payload_sources] != 160 ||
    [get_collection_size $profile2_payload_destinations] != 160
} {
    error "profile-2 bundled CDC register collection changed"
}

set_max_delay 12.500 \
    -from $profile2_payload_sources \
    -to $profile2_payload_destinations
set_false_path -hold \
    -from $profile2_payload_sources \
    -to $profile2_payload_destinations

# sys_reset_i asynchronously asserts both RX reset-release registers. Their
# deassertion is intentionally synchronized by the two-register chain, so cut
# only the asynchronous clear pins; retain normal timing between the registers.
set profile2_reset_clear_pins [get_pins -compatibility_mode {
    *rf_profile2_init_reset:U_rf_profile2_init_reset|rx_reset_meta|clrn
    *rf_profile2_init_reset:U_rf_profile2_init_reset|rx_reset_sync|clrn
}]

if {[get_collection_size $profile2_reset_clear_pins] != 2} {
    error "profile-2 reset synchronizer clear-pin collection changed"
}

set_false_path -to $profile2_reset_clear_pins

# The profile reset is released on the RX/write clock before it reaches the
# dual-clock sample FIFO.  That FIFO has READ_ACLR_SYNCH enabled, so its two
# read-side rdaclr registers deliberately receive an asynchronous clear whose
# release is synchronized internally to the FX3/read clock.  This is the
# read-side counterpart of the upstream wraclr exception for RX disable.
set profile2_fifo_reset_source [get_registers {
    *rf_profile2_init_reset:U_rf_profile2_init_reset|rx_reset_sync
}]
set profile2_fifo_read_clear_pins [get_pins -compatibility_mode {
    *rx_fifo:U_rx_sample_fifo*|dffpipe_*:rdaclr|*|clrn
}]

if {
    [get_collection_size $profile2_fifo_reset_source] != 1 ||
    [get_collection_size $profile2_fifo_read_clear_pins] != 2
} {
    error "profile-2 sample-FIFO read-clear collection changed"
}

set_false_path \
    -from $profile2_fifo_reset_source \
    -to $profile2_fifo_read_clear_pins
