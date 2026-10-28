-- Fixed profile 1: 20 MSps input, four nominal offsets -6/-2/+2/+6 MHz.
-- Every CIC output becomes eight raw complex transport slots (32 bytes):
--   0: 0x5246,0x4f42 magic; 1: flags,0x0001 profile;
--   2: index bits15:0,31:16; 3: bits47:32,63:48;
--   4..7: signed I,Q channel samples.
-- Flags: version 1 in15:12, per-channel clipping in11:8, sticky input/FIFO
-- loss in7, sticky unsupported-configuration/source-change in6, RX2 source in5.
-- Bits4:0 are zero. Index is accepted source sample, not RF/UTC time.
--
-- The actual ADC cannot stall: s_valid while the core is not ready sets loss.
-- The serializer itself honors m_ready and holds its current slot unchanged.
-- A complete record is snapshotted before serialization. Error indicators that
-- arise after that snapshot appear in the next record, so a clean record alone
-- cannot certify completeness. Reset begins a new epoch and clears sticky flags.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

library work;
use work.rf_observatory_p.all;

entity rf_channel_framer is
    port (
        clock : in std_logic;
        reset : in std_logic;
        s_valid : in std_logic;
        s_i, s_q : in signed(15 downto 0);
        source_rx2 : in std_logic;
        configuration_error : in std_logic;
        fifo_overflow : in std_logic;
        m_valid : out std_logic;
        m_ready : in std_logic;
        m_i, m_q : out signed(15 downto 0)
    );
end entity;

architecture rtl of rf_channel_framer is
    constant PROFILE_STEPS : phase_steps_t := (
        x"b3333333", x"e6666666", x"1999999a", x"4ccccccd"
    );
    signal core_ready, core_valid, core_output_ready : std_logic;
    signal core_i, core_q : iq_words_t;
    signal core_clip : std_logic_vector(3 downto 0);
    signal core_index : unsigned(63 downto 0);
    signal busy : std_logic := '0';
    signal slot : natural range 0 to 7 := 0;
    signal held_i, held_q : iq_words_t := (others => (others => '0'));
    signal held_index : unsigned(63 downto 0) := (others => '0');
    signal held_flags : std_logic_vector(15 downto 0) := (others => '0');
    signal loss, bad_config : std_logic := '0';
    signal source_known, selected_source : std_logic := '0';
begin
    bank : entity work.rf_channel_bank
        port map (clock, reset, PROFILE_STEPS, s_valid, core_ready, s_i, s_q,
            core_valid, core_output_ready, core_i, core_q, core_clip, core_index);
    core_output_ready <= not busy;
    m_valid <= busy and not reset;

    process(all)
    begin
        m_i <= (others => '0');
        m_q <= (others => '0');
        case slot is
            when 0 => m_i <= signed'(x"5246"); m_q <= signed'(x"4f42");
            when 1 => m_i <= signed(held_flags); m_q <= to_signed(1,16);
            when 2 => m_i <= signed(held_index(15 downto 0)); m_q <= signed(held_index(31 downto 16));
            when 3 => m_i <= signed(held_index(47 downto 32)); m_q <= signed(held_index(63 downto 48));
            when others => m_i <= held_i(slot-4); m_q <= held_q(slot-4);
        end case;
    end process;

    process(clock)
        variable flags : std_logic_vector(15 downto 0);
    begin
        if rising_edge(clock) then
            if reset = '1' then
                busy <= '0'; slot <= 0;
                held_i <= (others => (others => '0'));
                held_q <= (others => (others => '0'));
                held_index <= (others => '0'); held_flags <= (others => '0');
                loss <= '0'; bad_config <= '0';
                source_known <= '0'; selected_source <= '0';
            else
                if fifo_overflow = '1' or (s_valid = '1' and core_ready = '0') then
                    loss <= '1';
                end if;
                if configuration_error = '1' then bad_config <= '1'; end if;
                if s_valid = '1' then
                    if source_known = '0' then
                        selected_source <= source_rx2;
                        source_known <= '1';
                    elsif selected_source /= source_rx2 then
                        bad_config <= '1';
                    end if;
                end if;
                if busy = '1' then
                    if m_ready = '1' then
                        if slot = 7 then busy <= '0'; slot <= 0;
                        else slot <= slot+1; end if;
                    end if;
                elsif core_valid = '1' then
                    held_i <= core_i; held_q <= core_q; held_index <= core_index;
                    flags := x"1000";
                    flags(11 downto 8) := core_clip;
                    flags(7) := loss or fifo_overflow;
                    flags(6) := bad_config or configuration_error;
                    flags(5) := selected_source;
                    held_flags <= flags;
                    busy <= '1'; slot <= 0;
                end if;
            end if;
        end if;
    end process;
end architecture;
