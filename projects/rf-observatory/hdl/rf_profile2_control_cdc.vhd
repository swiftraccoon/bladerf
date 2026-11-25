-- Profile-2 configuration handshake between the Wishbone/system clock and the
-- AD9361 RX clock. The producer must hold apply_valid until apply_done. This
-- block captures the 160-bit request payload in the control domain, then holds
-- that bundled data stable from a full clock before request through response.
-- A four-phase request/response handshake carries only control levels and the
-- one-bit result. Keeping the producer busy through return-to-zero prevents a
-- fast following request from being lost.
--
-- control_reset and rx_reset must come from one image-reset event; bounded
-- assertion/deassertion skew is allowed, but isolated domain reset is not a
-- recovery protocol. Digital simulation cannot qualify metastability or the
-- eventual fitted timing constraint on the bundled payload.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_control_cdc is
    port (
        control_clock : in std_logic;
        control_reset : in std_logic;

        apply_valid : in std_logic;
        apply_words : in phase_steps_t;
        apply_epoch : in unsigned(31 downto 0);
        apply_done : out std_logic;
        apply_success : out std_logic;
        rx_active_sync : out std_logic;

        rx_clock : in std_logic;
        rx_reset : in std_logic;
        rx_enable : in std_logic;

        start_pulse : out std_logic;
        start_authorized : out std_logic;
        active_valid : out std_logic;
        active_words : out phase_steps_t;
        active_epoch : out unsigned(31 downto 0)
    );
end entity;

architecture rtl of rf_profile2_control_cdc is
    type control_state_t is (CONTROL_IDLE, CONTROL_PREPARE_REQUEST,
        CONTROL_WAIT_RESPONSE, CONTROL_WAIT_RESPONSE_RELEASE,
        CONTROL_WAIT_VALID_RELEASE);
    type rx_state_t is (RX_IDLE, RX_CAPTURE, RX_WAIT_REQUEST_RELEASE);

    signal control_state : control_state_t := CONTROL_IDLE;
    signal request_level : std_logic := '0';
    signal response_level_rx : std_logic := '0';
    signal response_level_sync : std_logic := '0';
    signal response_success_rx : std_logic := '0';
    signal response_success_sync : std_logic := '0';
    signal apply_done_i : std_logic := '0';
    signal apply_success_i : std_logic := '0';
    signal request_words_control : phase_steps_t := (others => (others => '0'));
    signal request_epoch_control : unsigned(31 downto 0) := (others => '0');

    signal request_level_sync : std_logic := '0';
    signal rx_state : rx_state_t := RX_IDLE;

    signal armed_words_rx : phase_steps_t := (others => (others => '0'));
    signal armed_epoch_rx : unsigned(31 downto 0) := (others => '0');
    signal token_armed_rx : std_logic := '0';
    signal previous_rx_enable : std_logic := '0';
    signal start_pulse_i : std_logic := '0';
    signal start_authorized_i : std_logic := '0';
    signal active_valid_i : std_logic := '0';
    signal active_words_i : phase_steps_t := (others => (others => '0'));
    signal active_epoch_i : unsigned(31 downto 0) := (others => '0');
begin
    apply_done <= apply_done_i;
    apply_success <= apply_success_i;
    start_pulse <= start_pulse_i;
    start_authorized <= start_authorized_i;
    active_valid <= active_valid_i;
    active_words <= active_words_i;
    active_epoch <= active_epoch_i;

    request_level_synchronizer : entity work.synchronizer
        generic map (RESET_LEVEL => '0')
        port map (
            reset => rx_reset,
            clock => rx_clock,
            async => request_level,
            sync => request_level_sync
        );

    response_level_synchronizer : entity work.synchronizer
        generic map (RESET_LEVEL => '0')
        port map (
            reset => control_reset,
            clock => control_clock,
            async => response_level_rx,
            sync => response_level_sync
        );

    response_value_sync : entity work.synchronizer
        generic map (RESET_LEVEL => '0')
        port map (
            reset => control_reset,
            clock => control_clock,
            async => response_success_rx,
            sync => response_success_sync
        );

    rx_active_level_sync : entity work.synchronizer
        generic map (RESET_LEVEL => '0')
        port map (
            reset => control_reset,
            clock => control_clock,
            async => rx_enable,
            sync => rx_active_sync
        );

    control_handshake : process(control_clock, control_reset)
    begin
        if control_reset = '1' then
            control_state <= CONTROL_IDLE;
            request_level <= '0';
            apply_done_i <= '0';
            apply_success_i <= '0';
            request_words_control <= (others => (others => '0'));
            request_epoch_control <= (others => '0');
        elsif rising_edge(control_clock) then
            apply_done_i <= '0';
            case control_state is
                when CONTROL_IDLE =>
                    apply_success_i <= '0';
                    if apply_valid = '1' then
                        request_words_control <= apply_words;
                        request_epoch_control <= apply_epoch;
                        control_state <= CONTROL_PREPARE_REQUEST;
                    end if;

                when CONTROL_PREPARE_REQUEST =>
                    -- Give all 160 bundled payload bits one full source clock
                    -- of stability before the request level can cross domains.
                    request_level <= '1';
                    control_state <= CONTROL_WAIT_RESPONSE;

                when CONTROL_WAIT_RESPONSE =>
                    if response_level_sync = '1' then
                        request_level <= '0';
                        control_state <= CONTROL_WAIT_RESPONSE_RELEASE;
                    end if;

                when CONTROL_WAIT_RESPONSE_RELEASE =>
                    if response_level_sync = '0' then
                        -- The separately synchronized result has remained
                        -- stable through the complete four-phase exchange.
                        apply_success_i <= response_success_sync;
                        apply_done_i <= '1';
                        control_state <= CONTROL_WAIT_VALID_RELEASE;
                    end if;

                when CONTROL_WAIT_VALID_RELEASE =>
                    -- The register samples apply_done one clock after this block
                    -- raises it. Do not reinterpret its still-high apply_valid as
                    -- another request while that level is being released.
                    if apply_valid = '0' then
                        apply_success_i <= '0';
                        control_state <= CONTROL_IDLE;
                    end if;
            end case;
        end if;
    end process;

    rx_handshake_and_start : process(rx_clock, rx_reset)
    begin
        if rx_reset = '1' then
            rx_state <= RX_IDLE;
            response_level_rx <= '0';
            response_success_rx <= '0';
            armed_words_rx <= (others => (others => '0'));
            armed_epoch_rx <= (others => '0');
            token_armed_rx <= '0';
            previous_rx_enable <= '0';
            start_pulse_i <= '0';
            start_authorized_i <= '0';
            active_valid_i <= '0';
            active_words_i <= (others => (others => '0'));
            active_epoch_i <= (others => '0');
        elsif rising_edge(rx_clock) then
            start_pulse_i <= '0';
            start_authorized_i <= '0';
            previous_rx_enable <= rx_enable;

            if rx_enable = '1' and previous_rx_enable = '0' then
                start_pulse_i <= '1';
                if token_armed_rx = '1' then
                    start_authorized_i <= '1';
                    active_valid_i <= '1';
                    active_words_i <= armed_words_rx;
                    active_epoch_i <= armed_epoch_rx;
                    token_armed_rx <= '0';
                else
                    active_valid_i <= '0';
                    active_words_i <= (others => (others => '0'));
                    active_epoch_i <= (others => '0');
                end if;
            elsif rx_enable = '0' then
                active_valid_i <= '0';
                active_words_i <= (others => (others => '0'));
                active_epoch_i <= (others => '0');
            end if;

            case rx_state is
                when RX_IDLE =>
                    if request_level_sync = '1' then
                        rx_state <= RX_CAPTURE;
                    end if;

                when RX_CAPTURE =>
                    -- RX-enable wins any same-edge race. While stopped, a new
                    -- successful apply replaces the one armed configuration;
                    -- it does not queue a second future start.
                    if rx_enable = '0' then
                        armed_words_rx <= request_words_control;
                        armed_epoch_rx <= request_epoch_control;
                        token_armed_rx <= '1';
                        response_success_rx <= '1';
                    else
                        response_success_rx <= '0';
                    end if;
                    response_level_rx <= '1';
                    rx_state <= RX_WAIT_REQUEST_RELEASE;

                when RX_WAIT_REQUEST_RELEASE =>
                    if request_level_sync = '0' then
                        response_level_rx <= '0';
                        rx_state <= RX_IDLE;
                    end if;
            end case;
        end if;
    end process;
end architecture;
