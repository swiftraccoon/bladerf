-- Profile-2 file-driven transport verification. The vector format intentionally
-- matches rf_channel_bank_tb: one line containing four hexadecimal phase words,
-- followed by signed decimal I/Q input pairs. The output is one hexadecimal
-- transport I/Q slot per line, including the leading CONFIG record.
--
-- This bench fixes the epoch and source so the maintained verifier has one
-- deterministic CONFIG oracle. Periodic consumer stalls also check that a
-- blocked ready/valid slot remains stable.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use ieee.std_logic_textio.all;
use std.textio.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_framer_tb is
    generic (
        G_INPUT_FILE : string := "input.txt";
        G_OUTPUT_FILE : string := "output.txt"
    );
end entity;

architecture simulation of rf_profile2_framer_tb is
    constant GOLDEN_WORDS : phase_steps_t := (
        x"01234567", x"89abcdef", x"80000001", x"deadbeef"
    );
    constant GOLDEN_EPOCH : unsigned(31 downto 0) := x"12345678";

    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal start_pulse : std_logic := '0';
    signal s_valid : std_logic := '0';
    signal s_ready : std_logic;
    signal s_i, s_q : signed(15 downto 0) := (others => '0');
    signal m_valid : std_logic;
    signal m_ready : std_logic := '0';
    signal m_i, m_q : signed(15 downto 0);
    signal emitted : natural := 0;
begin
    clock <= not clock after 5 ns;

    dut : entity work.rf_profile2_framer
        port map (
            clock => clock,
            reset => reset,
            start_pulse => start_pulse,
            start_authorized => '1',
            active_valid => '1',
            active_words => GOLDEN_WORDS,
            active_epoch => GOLDEN_EPOCH,
            s_valid => s_valid,
            s_ready => s_ready,
            s_i => s_i,
            s_q => s_q,
            source_rx2 => '1',
            configuration_error => '0',
            transport_loss => '0',
            m_valid => m_valid,
            m_ready => m_ready,
            m_i => m_i,
            m_q => m_q
        );

    producer : process
        file input_file : text open read_mode is G_INPUT_FILE;
        variable row : line;
        variable phase_word : std_logic_vector(31 downto 0);
        variable value_i, value_q : integer;
        variable samples, expected_slots : natural := 0;
    begin
        assert not endfile(input_file)
            report "missing profile-2 oscillator configuration" severity failure;
        readline(input_file, row);
        for channel in 0 to 3 loop
            hread(row, phase_word);
            assert unsigned(phase_word) = GOLDEN_WORDS(channel)
                report "profile-2 framer vector uses the wrong phase words"
                severity failure;
        end loop;

        for n in 0 to 4 loop
            wait until rising_edge(clock);
        end loop;
        wait until falling_edge(clock);
        reset <= '0';
        start_pulse <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        start_pulse <= '0';

        while not endfile(input_file) loop
            readline(input_file, row);
            read(row, value_i);
            read(row, value_q);
            assert value_i >= -32768 and value_i <= 32767
                and value_q >= -32768 and value_q <= 32767
                report "profile-2 input component exceeds signed 16-bit range"
                severity failure;
            if samples < 20 then
                assert value_i = 0 and value_q = 0
                    report "profile-2 golden fixture requires 20 leading zero samples"
                    severity failure;
            end if;
            s_i <= to_signed(value_i, 16);
            s_q <= to_signed(value_q, 16);
            s_valid <= '1';
            loop
                wait until rising_edge(clock);
                exit when s_ready = '1';
            end loop;
            samples := samples + 1;
            wait until falling_edge(clock);
            s_valid <= '0';
            if samples mod 17 = 0 then
                wait until falling_edge(clock);
                wait until falling_edge(clock);
            end if;
        end loop;

        expected_slots := 8 + (samples / 20) * 8;
        for n in 0 to 2000 loop
            exit when emitted = expected_slots;
            wait until rising_edge(clock);
        end loop;
        assert emitted = expected_slots
            report "profile-2 framer did not emit the expected record count"
            severity failure;
        for n in 0 to 63 loop
            wait until rising_edge(clock);
        end loop;
        assert emitted = expected_slots
            report "profile-2 framer emitted an extra record" severity failure;
        report "profile-2 channel-framer vector simulation passed" severity note;
        finish;
    end process;

    consumer : process(clock)
        file output_file : text open write_mode is G_OUTPUT_FILE;
        variable row : line;
        variable cycles, count : natural := 0;
        variable held : boolean := false;
        variable held_i, held_q : signed(15 downto 0);
    begin
        if rising_edge(clock) then
            cycles := cycles + 1;
            assert cycles < 20000000
                report "profile-2 framer cycle budget exceeded" severity failure;
            if reset = '1' then
                m_ready <= '0';
                held := false;
            else
                if cycles mod 41 = 13 or cycles mod 41 = 14 then
                    m_ready <= '0';
                else
                    m_ready <= '1';
                end if;

                if held then
                    assert m_valid = '1' and m_i = held_i and m_q = held_q
                        report "profile-2 transport slot changed under backpressure"
                        severity failure;
                end if;
                held := m_valid = '1' and m_ready = '0';
                held_i := m_i;
                held_q := m_q;

                if m_valid = '1' and m_ready = '1' then
                    -- Literal CONFIG oracle: no constants from the DUT define
                    -- these values or their slot ordering.
                    case count is
                        when 0 =>
                            assert std_logic_vector(m_i) = x"5246"
                                and std_logic_vector(m_q) = x"4f42"
                                report "profile-2 CONFIG magic mismatch" severity failure;
                        when 1 =>
                            assert std_logic_vector(m_i) = x"2038"
                                and std_logic_vector(m_q) = x"0002"
                                report "profile-2 CONFIG flags/profile mismatch"
                                severity failure;
                        when 2 =>
                            assert std_logic_vector(m_i) = x"5678"
                                and std_logic_vector(m_q) = x"1234"
                                report "profile-2 CONFIG epoch endianness mismatch"
                                severity failure;
                        when 3 =>
                            assert std_logic_vector(m_i) = x"2d00"
                                and std_logic_vector(m_q) = x"0131"
                                report "profile-2 CONFIG rate endianness mismatch"
                                severity failure;
                        when 4 =>
                            assert std_logic_vector(m_i) = x"4567"
                                and std_logic_vector(m_q) = x"0123"
                                report "profile-2 CONFIG word 0 mismatch" severity failure;
                        when 5 =>
                            assert std_logic_vector(m_i) = x"cdef"
                                and std_logic_vector(m_q) = x"89ab"
                                report "profile-2 CONFIG word 1 mismatch" severity failure;
                        when 6 =>
                            assert std_logic_vector(m_i) = x"0001"
                                and std_logic_vector(m_q) = x"8000"
                                report "profile-2 CONFIG word 2 mismatch" severity failure;
                        when 7 =>
                            assert std_logic_vector(m_i) = x"beef"
                                and std_logic_vector(m_q) = x"dead"
                                report "profile-2 CONFIG word 3 mismatch" severity failure;
                        when 8 =>
                            assert std_logic_vector(m_i) = x"5246"
                                and std_logic_vector(m_q) = x"4f42"
                                report "profile-2 first DATA magic mismatch" severity failure;
                        when 9 =>
                            assert std_logic_vector(m_i) = x"2028"
                                and std_logic_vector(m_q) = x"0002"
                                report "profile-2 first DATA flags/profile mismatch"
                                severity failure;
                        when 10 =>
                            assert std_logic_vector(m_i) = x"0013"
                                and std_logic_vector(m_q) = x"0000"
                                report "profile-2 first DATA index low word mismatch"
                                severity failure;
                        when 11 =>
                            assert std_logic_vector(m_i) = x"0000"
                                and std_logic_vector(m_q) = x"0000"
                                report "profile-2 first DATA index high word mismatch"
                                severity failure;
                        when 12 to 15 =>
                            assert std_logic_vector(m_i) = x"0000"
                                and std_logic_vector(m_q) = x"0000"
                                report "profile-2 first DATA sample payload mismatch"
                                severity failure;
                        when others => null;
                    end case;

                    hwrite(row, std_logic_vector(m_i));
                    write(row, string'(" "));
                    hwrite(row, std_logic_vector(m_q));
                    writeline(output_file, row);
                    count := count + 1;
                    emitted <= count;
                end if;
            end if;
        end if;
    end process;
end architecture;
