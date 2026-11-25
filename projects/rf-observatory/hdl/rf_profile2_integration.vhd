-- Profile-2 control, channel-bank transport and native-FIFO write boundary.
-- The two reset classes are intentionally distinct:
--
-- * control_reset/profile_rx_reset are one image-lifetime event. They clear the
--   committed configuration and one-shot start token, so a normal stream-start
--   reset must not drive them.
-- * stream_reset and a stopped rx_enable clear only epoch-local framing,
--   numerical and packing state. A later start consumes a previously armed
--   token and emits CONFIG again.
--
-- The wrapper owns the complete path from the terminating Wishbone slave to a
-- single-driver 64-bit RX-FIFO write interface. fifo_full is backpressure, not
-- a transport-loss event. Once DATA is open, the framer itself reports an
-- unstallable ADC sample presented while that backpressure reaches the bank.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_integration is
    port (
        control_clock : in std_logic;
        control_reset : in std_logic;

        wb_address : in std_logic_vector(31 downto 0);
        wb_write_data : in std_logic_vector(31 downto 0);
        wb_read_data : out std_logic_vector(31 downto 0);
        wb_write : in std_logic;
        wb_select : in std_logic;
        wb_strobe : in std_logic;
        wb_cycle : in std_logic;
        wb_ack : out std_logic;

        rx_clock : in std_logic;
        profile_rx_reset : in std_logic;
        stream_reset : in std_logic;
        rx_enable : in std_logic;

        s_valid : in std_logic;
        s_ready : out std_logic;
        s_i : in signed(15 downto 0);
        s_q : in signed(15 downto 0);
        source_rx2 : in std_logic;
        configuration_error : in std_logic;
        transport_loss : in std_logic;

        fifo_full : in std_logic;
        fifo_write : out std_logic;
        fifo_data : out std_logic_vector(63 downto 0)
    );
end entity;

architecture rtl of rf_profile2_integration is
    signal apply_valid, apply_done, apply_success : std_logic;
    signal apply_words : phase_steps_t;
    signal apply_epoch : unsigned(31 downto 0);
    signal rx_active_sync : std_logic;

    signal start_pulse, start_authorized : std_logic;
    signal active_valid : std_logic;
    signal active_words : phase_steps_t;
    signal active_epoch : unsigned(31 downto 0);

    signal epoch_reset : std_logic;
    signal framed_valid, framed_ready : std_logic;
    signal framed_i, framed_q : signed(15 downto 0);
begin
    epoch_reset <= profile_rx_reset or stream_reset or not rx_enable;

    registers : entity work.rf_profile2_wb_registers
        port map (
            clock => control_clock,
            reset => control_reset,
            wb_address => wb_address,
            wb_write_data => wb_write_data,
            wb_read_data => wb_read_data,
            wb_write => wb_write,
            wb_select => wb_select,
            wb_strobe => wb_strobe,
            wb_cycle => wb_cycle,
            wb_ack => wb_ack,
            rx_active_sync => rx_active_sync,
            apply_valid => apply_valid,
            apply_words => apply_words,
            apply_epoch => apply_epoch,
            apply_done => apply_done,
            apply_success => apply_success
        );

    control_bridge : entity work.rf_profile2_control_cdc
        port map (
            control_clock => control_clock,
            control_reset => control_reset,
            apply_valid => apply_valid,
            apply_words => apply_words,
            apply_epoch => apply_epoch,
            apply_done => apply_done,
            apply_success => apply_success,
            rx_active_sync => rx_active_sync,
            rx_clock => rx_clock,
            rx_reset => profile_rx_reset,
            rx_enable => rx_enable,
            start_pulse => start_pulse,
            start_authorized => start_authorized,
            active_valid => active_valid,
            active_words => active_words,
            active_epoch => active_epoch
        );

    framer : entity work.rf_profile2_framer
        port map (
            clock => rx_clock,
            reset => epoch_reset,
            start_pulse => start_pulse,
            start_authorized => start_authorized,
            active_valid => active_valid,
            active_words => active_words,
            active_epoch => active_epoch,
            s_valid => s_valid,
            s_ready => s_ready,
            s_i => s_i,
            s_q => s_q,
            source_rx2 => source_rx2,
            configuration_error => configuration_error,
            transport_loss => transport_loss,
            m_valid => framed_valid,
            m_ready => framed_ready,
            m_i => framed_i,
            m_q => framed_q
        );

    packer : entity work.rf_profile2_slot_packer
        port map (
            clock => rx_clock,
            reset => epoch_reset,
            m_valid => framed_valid,
            m_ready => framed_ready,
            m_i => framed_i,
            m_q => framed_q,
            fifo_full => fifo_full,
            fifo_write => fifo_write,
            fifo_data => fifo_data
        );
end architecture;
