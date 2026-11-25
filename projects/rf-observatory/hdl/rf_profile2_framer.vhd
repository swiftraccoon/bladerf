-- Profile-2 CONFIG-first channel bank and transport serializer. Every idle-
-- epoch RX start emits exactly one eight-slot CONFIG record before any sample
-- is admitted to the numerical bank. An unauthorized or invalid start emits
-- the diagnostic CONFIG and then remains silent until reset.
--
-- Output is a conventional ready/valid stream. While m_valid is asserted and
-- m_ready is clear, the current slot and every snapshotted record field remain
-- unchanged. The downstream wrapper must make m_ready mean that the slot will
-- be retained; serializer completion alone is not FIFO-persistence evidence.
--
-- s_ready denotes actual admission to rf_channel_bank. It remains low while
-- CONFIG is pending and after an invalid start. Physical samples presented in
-- that deliberate startup interval are discarded without setting input loss.
-- Once DATA is enabled, an unstallable input presented while s_ready is low sets
-- sticky loss. Reset discards every partial record and all numerical state; the
-- integration wrapper must coherently clear its downstream packer/FIFO too.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_framer is
    port (
        clock : in std_logic;
        reset : in std_logic;

        start_pulse : in std_logic;
        start_authorized : in std_logic;
        active_valid : in std_logic;
        active_words : in phase_steps_t;
        active_epoch : in unsigned(31 downto 0);

        s_valid : in std_logic;
        s_ready : out std_logic;
        s_i : in signed(15 downto 0);
        s_q : in signed(15 downto 0);
        source_rx2 : in std_logic;
        configuration_error : in std_logic;
        transport_loss : in std_logic;

        m_valid : out std_logic;
        m_ready : in std_logic;
        m_i : out signed(15 downto 0);
        m_q : out signed(15 downto 0)
    );
end entity;

architecture rtl of rf_profile2_framer is
    constant REQUIRED_SOURCE_RATE : unsigned(31 downto 0) :=
        to_unsigned(20_000_000, 32);

    type state_t is (
        WAIT_START,
        EMIT_CONFIG,
        DATA_WAIT_GROUP,
        EMIT_DATA,
        DIAGNOSTIC_DONE
    );

    signal state : state_t := WAIT_START;
    signal slot : natural range 0 to 7 := 0;

    signal epoch_usable : std_logic := '0';
    signal startup_transport_loss : std_logic := '0';
    signal epoch_words : phase_steps_t := (others => (others => '0'));
    signal epoch_number : unsigned(31 downto 0) := (others => '0');
    signal epoch_source_rx2 : std_logic := '0';
    signal config_flags : std_logic_vector(15 downto 0) := (others => '0');

    signal loss_sticky : std_logic := '0';
    signal config_sticky : std_logic := '0';

    signal bank_reset : std_logic;
    signal bank_s_valid : std_logic;
    signal bank_s_ready : std_logic;
    signal bank_m_valid : std_logic;
    signal bank_m_ready : std_logic;
    signal bank_i : iq_words_t;
    signal bank_q : iq_words_t;
    signal bank_clipped : std_logic_vector(3 downto 0);
    signal bank_index : unsigned(63 downto 0);

    signal held_i : iq_words_t := (others => (others => '0'));
    signal held_q : iq_words_t := (others => (others => '0'));
    signal held_index : unsigned(63 downto 0) := (others => '0');
    signal held_flags : std_logic_vector(15 downto 0) := (others => '0');

    signal data_phase : std_logic;
begin
    data_phase <= '1' when state = DATA_WAIT_GROUP or state = EMIT_DATA else '0';
    bank_reset <= reset or not data_phase;
    bank_s_valid <= s_valid when data_phase = '1' else '0';
    bank_m_ready <= '1' when state = DATA_WAIT_GROUP else '0';

    s_ready <= bank_s_ready when data_phase = '1' and reset = '0' else '0';
    m_valid <= '1' when (state = EMIT_CONFIG or state = EMIT_DATA)
        and reset = '0' else '0';

    bank : entity work.rf_channel_bank
        port map (
            clock => clock,
            reset => bank_reset,
            phase_steps => epoch_words,
            s_valid => bank_s_valid,
            s_ready => bank_s_ready,
            s_i => s_i,
            s_q => s_q,
            m_valid => bank_m_valid,
            m_ready => bank_m_ready,
            m_i => bank_i,
            m_q => bank_q,
            m_clipped => bank_clipped,
            m_last_input_index => bank_index
        );

    serialize : process(all)
    begin
        m_i <= (others => '0');
        m_q <= (others => '0');

        if state = EMIT_CONFIG then
            case slot is
                when 0 =>
                    m_i <= signed'(x"5246");
                    m_q <= signed'(x"4f42");
                when 1 =>
                    m_i <= signed(config_flags);
                    m_q <= to_signed(2, 16);
                when 2 =>
                    m_i <= signed(epoch_number(15 downto 0));
                    m_q <= signed(epoch_number(31 downto 16));
                when 3 =>
                    m_i <= signed(REQUIRED_SOURCE_RATE(15 downto 0));
                    m_q <= signed(REQUIRED_SOURCE_RATE(31 downto 16));
                when others =>
                    m_i <= signed(epoch_words(slot - 4)(15 downto 0));
                    m_q <= signed(epoch_words(slot - 4)(31 downto 16));
            end case;
        elsif state = EMIT_DATA then
            case slot is
                when 0 =>
                    m_i <= signed'(x"5246");
                    m_q <= signed'(x"4f42");
                when 1 =>
                    m_i <= signed(held_flags);
                    m_q <= to_signed(2, 16);
                when 2 =>
                    m_i <= signed(held_index(15 downto 0));
                    m_q <= signed(held_index(31 downto 16));
                when 3 =>
                    m_i <= signed(held_index(47 downto 32));
                    m_q <= signed(held_index(63 downto 48));
                when others =>
                    m_i <= held_i(slot - 4);
                    m_q <= held_q(slot - 4);
            end case;
        end if;
    end process;

    control : process(clock)
        variable flags : std_logic_vector(15 downto 0);
        variable input_loss_now : std_logic;
        variable config_error_now : std_logic;
    begin
        if rising_edge(clock) then
            if reset = '1' then
                state <= WAIT_START;
                slot <= 0;
                epoch_usable <= '0';
                startup_transport_loss <= '0';
                epoch_words <= (others => (others => '0'));
                epoch_number <= (others => '0');
                epoch_source_rx2 <= '0';
                config_flags <= (others => '0');
                loss_sticky <= '0';
                config_sticky <= '0';
                held_i <= (others => (others => '0'));
                held_q <= (others => (others => '0'));
                held_index <= (others => '0');
                held_flags <= (others => '0');
            else
                input_loss_now := '0';
                config_error_now := '0';
                if data_phase = '1' then
                    if s_valid = '1' and bank_s_ready = '0' then
                        input_loss_now := '1';
                    end if;
                    if configuration_error = '1'
                        or source_rx2 /= epoch_source_rx2
                        or start_pulse = '1' then
                        config_error_now := '1';
                    end if;

                    if input_loss_now = '1' or transport_loss = '1' then
                        loss_sticky <= '1';
                    end if;
                    if config_error_now = '1' then
                        config_sticky <= '1';
                    end if;
                end if;

                case state is
                    when WAIT_START =>
                        if start_pulse = '1' then
                            slot <= 0;
                            epoch_source_rx2 <= source_rx2;
                            loss_sticky <= '0';
                            config_sticky <= '0';
                            startup_transport_loss <= transport_loss;
                            held_i <= (others => (others => '0'));
                            held_q <= (others => (others => '0'));
                            held_index <= (others => '0');
                            held_flags <= (others => '0');

                            flags := x"2010";
                            flags(5) := source_rx2;
                            if start_authorized = '1' and active_valid = '1'
                                and active_epoch /= unsigned'(x"00000000")
                                and configuration_error = '0'
                                and transport_loss = '0' then
                                epoch_usable <= '1';
                                epoch_words <= active_words;
                                epoch_number <= active_epoch;
                                flags(3 downto 0) :=
                                    std_logic_vector(active_epoch(3 downto 0));
                            else
                                epoch_usable <= '0';
                                epoch_words <= (others => (others => '0'));
                                epoch_number <= (others => '0');
                                flags(6) := '1';
                                flags(3 downto 0) := (others => '0');
                            end if;
                            config_flags <= flags;
                            state <= EMIT_CONFIG;
                        end if;

                    when EMIT_CONFIG =>
                        -- A new start is never allowed to replace the tuple being
                        -- serialized. Preserve it and fail closed after CONFIG;
                        -- no DATA may follow an in-flight lifecycle violation.
                        if start_pulse = '1' or configuration_error = '1'
                            or source_rx2 /= epoch_source_rx2 then
                            config_sticky <= '1';
                        end if;
                        if transport_loss = '1' then
                            startup_transport_loss <= '1';
                        end if;

                        if m_ready = '1' then
                            if slot = 7 then
                                slot <= 0;
                                if epoch_usable = '1'
                                    and startup_transport_loss = '0'
                                    and transport_loss = '0'
                                    and config_sticky = '0'
                                    and start_pulse = '0'
                                    and configuration_error = '0'
                                    and source_rx2 = epoch_source_rx2 then
                                    state <= DATA_WAIT_GROUP;
                                else
                                    state <= DIAGNOSTIC_DONE;
                                end if;
                            else
                                slot <= slot + 1;
                            end if;
                        end if;

                    when DATA_WAIT_GROUP =>
                        if bank_m_valid = '1' then
                            held_i <= bank_i;
                            held_q <= bank_q;
                            held_index <= bank_index;

                            flags := x"2000";
                            flags(11 downto 8) := bank_clipped;
                            flags(7) := loss_sticky or transport_loss
                                or input_loss_now;
                            flags(6) := config_sticky or config_error_now;
                            flags(5) := epoch_source_rx2;
                            flags(3 downto 0) :=
                                std_logic_vector(epoch_number(3 downto 0));
                            held_flags <= flags;
                            slot <= 0;
                            state <= EMIT_DATA;
                        end if;

                    when EMIT_DATA =>
                        if m_ready = '1' then
                            if slot = 7 then
                                slot <= 0;
                                state <= DATA_WAIT_GROUP;
                            else
                                slot <= slot + 1;
                            end if;
                        end if;

                    when DIAGNOSTIC_DONE =>
                        -- A stop/reset is required before another start. Ignore
                        -- any extra pulse rather than changing the emitted tuple.
                        null;
                end case;
            end if;
        end if;
    end process;
end architecture;
