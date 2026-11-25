-- Always-terminating profile-2 register slave for the pinned Nuand bridge.
-- Capture belongs to cyc/stb; sel is the later response-wait indication.
-- The apply payload remains stable while apply_valid is high.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;
use work.rf_profile2_p.all;

entity rf_profile2_wb_registers is
    port (
        clock : in std_logic;
        reset : in std_logic;

        wb_address : in std_logic_vector(31 downto 0);
        wb_write_data : in std_logic_vector(31 downto 0);
        wb_read_data : out std_logic_vector(31 downto 0);
        wb_write : in std_logic;
        wb_select : in std_logic;
        wb_strobe : in std_logic;
        wb_cycle : in std_logic;
        wb_ack : out std_logic;

        rx_active_sync : in std_logic;
        apply_valid : out std_logic;
        apply_words : out phase_steps_t;
        apply_epoch : out unsigned(31 downto 0);
        apply_done : in std_logic;
        apply_success : in std_logic
    );
end entity;

architecture rtl of rf_profile2_wb_registers is
    type bus_state_t is (IDLE, WAIT_SELECT, RESPOND, WAIT_RELEASE);

    signal bus_state : bus_state_t := IDLE;
    signal saved_address : unsigned(31 downto 0) := (others => '0');
    signal saved_data : std_logic_vector(31 downto 0) := (others => '0');
    signal saved_write : std_logic := '0';
    signal response_data : std_logic_vector(31 downto 0) := (others => '0');

    signal shadow_words : phase_steps_t := (others => (others => '0'));
    signal written_mask : std_logic_vector(3 downto 0) := (others => '0');
    signal frozen_words : phase_steps_t := (others => (others => '0'));
    signal frozen_epoch : unsigned(31 downto 0) := (others => '0');
    signal applied_words : phase_steps_t := (others => (others => '0'));
    signal applied_epoch : unsigned(31 downto 0) := (others => '0');
    signal apply_pending : std_logic := '0';
    signal previous_apply_done : std_logic := '0';

    signal outcome_valid : std_logic := '0';
    signal outcome_success : std_logic := '0';
    signal outcome_reason : p2_reason_t := P2_REASON_NONE;
    signal outcome_epoch : unsigned(31 downto 0) := (others => '0');
begin
    wb_ack <= '1' when bus_state = RESPOND else '0';
    wb_read_data <= response_data;
    apply_valid <= apply_pending;
    apply_words <= frozen_words;
    apply_epoch <= frozen_epoch;

    process(clock, reset)
        variable address : natural range 0 to 15;
        variable next_epoch : unsigned(31 downto 0);
        variable commit_reason : p2_reason_t;
    begin
        if reset = '1' then
            bus_state <= IDLE;
            saved_address <= (others => '0');
            saved_data <= (others => '0');
            saved_write <= '0';
            response_data <= (others => '0');
            shadow_words <= (others => (others => '0'));
            written_mask <= (others => '0');
            frozen_words <= (others => (others => '0'));
            frozen_epoch <= (others => '0');
            applied_words <= (others => (others => '0'));
            applied_epoch <= (others => '0');
            apply_pending <= '0';
            previous_apply_done <= '0';
            outcome_valid <= '0';
            outcome_success <= '0';
            outcome_reason <= P2_REASON_NONE;
            outcome_epoch <= (others => '0');
        elsif rising_edge(clock) then
            previous_apply_done <= apply_done;
            if apply_done = '1' and previous_apply_done = '0'
                and apply_pending = '1' then
                apply_pending <= '0';
                outcome_valid <= '1';
                outcome_epoch <= frozen_epoch;
                if apply_success = '1' then
                    applied_words <= frozen_words;
                    applied_epoch <= frozen_epoch;
                    outcome_success <= '1';
                    outcome_reason <= P2_REASON_NONE;
                else
                    outcome_success <= '0';
                    outcome_reason <= P2_REASON_APPLY_REJECTED;
                end if;
            end if;

            case bus_state is
                when IDLE =>
                    if wb_cycle = '1' and wb_strobe = '1' then
                        saved_address <= unsigned(wb_address);
                        saved_data <= wb_write_data;
                        saved_write <= wb_write;
                        bus_state <= WAIT_SELECT;
                    end if;

                when WAIT_SELECT =>
                    if wb_select = '1' then
                        response_data <= (others => '0');
                        if saved_address(31 downto 4) = 0 then
                            address := to_integer(saved_address(3 downto 0));
                            if saved_write = '1' then
                                case address is
                                    when P2_ADDR_SHADOW_FIRST to P2_ADDR_SHADOW_FIRST + 3 =>
                                        shadow_words(address - P2_ADDR_SHADOW_FIRST) <=
                                            unsigned(saved_data);
                                        written_mask(address - P2_ADDR_SHADOW_FIRST) <= '1';

                                    when P2_ADDR_COMMIT =>
                                        next_epoch := p2_candidate_epoch(applied_epoch);
                                        commit_reason := P2_REASON_NONE;
                                        if apply_pending = '1' then
                                            null;
                                        else
                                            commit_reason := p2_commit_reason(
                                                saved_data, rx_active_sync,
                                                written_mask, applied_epoch
                                            );
                                        end if;
                                        if apply_pending = '0'
                                            and commit_reason /= P2_REASON_NONE then
                                            outcome_valid <= '1';
                                            outcome_success <= '0';
                                            outcome_reason <= commit_reason;
                                            outcome_epoch <= next_epoch;
                                        elsif apply_pending = '0' then
                                            frozen_words <= shadow_words;
                                            frozen_epoch <= next_epoch;
                                            apply_pending <= '1';
                                            written_mask <= (others => '0');
                                            outcome_valid <= '0';
                                            outcome_success <= '0';
                                            outcome_reason <= P2_REASON_NONE;
                                            outcome_epoch <= next_epoch;
                                        end if;

                                    when others =>
                                        null;
                                end case;
                            else
                                case address is
                                    when P2_ADDR_ID =>
                                        response_data <= P2_ID;
                                    when P2_ADDR_CAPABILITIES =>
                                        response_data <= P2_CAPABILITIES;
                                    when P2_ADDR_SHADOW_FIRST to P2_ADDR_SHADOW_FIRST + 3 =>
                                        response_data <= std_logic_vector(
                                            shadow_words(address - P2_ADDR_SHADOW_FIRST)
                                        );
                                    when P2_ADDR_COMMIT =>
                                        response_data <= (others => '0');
                                    when P2_ADDR_STATUS =>
                                        response_data <= p2_status(
                                            apply_pending, outcome_valid, outcome_success,
                                            rx_active_sync, outcome_reason, written_mask
                                        );
                                    when P2_ADDR_APPLIED_EPOCH =>
                                        response_data <= std_logic_vector(applied_epoch);
                                    when P2_ADDR_OUTCOME_EPOCH =>
                                        response_data <= std_logic_vector(outcome_epoch);
                                    when P2_ADDR_APPLIED_FIRST to P2_ADDR_APPLIED_FIRST + 3 =>
                                        response_data <= std_logic_vector(
                                            applied_words(address - P2_ADDR_APPLIED_FIRST)
                                        );
                                    when others =>
                                        response_data <= P2_UNKNOWN_READ;
                                end case;
                            end if;
                        elsif saved_write = '0' then
                            response_data <= P2_UNKNOWN_READ;
                        end if;
                        bus_state <= RESPOND;
                    end if;

                when RESPOND =>
                    bus_state <= WAIT_RELEASE;

                when WAIT_RELEASE =>
                    if wb_select = '0' then
                        if wb_cycle = '1' and wb_strobe = '1' then
                            saved_address <= unsigned(wb_address);
                            saved_data <= wb_write_data;
                            saved_write <= wb_write;
                            bus_state <= WAIT_SELECT;
                        else
                            bus_state <= IDLE;
                        end if;
                    end if;
            end case;
        end if;
    end process;
end architecture;
