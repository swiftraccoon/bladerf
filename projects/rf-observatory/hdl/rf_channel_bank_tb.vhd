-- File-driven verification, intentionally excluded from the synthesis sources.
-- Input: four hexadecimal phase words, then signed decimal I Q pairs.
-- Output: last accepted input index, then I Q clipped for each of four channels.
-- The producer inserts gaps and the consumer applies long periodic stalls.
-- Assertions check stable held output and first/next output index. A trailing
-- group shorter than 20 inputs is intentionally not emitted.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use ieee.std_logic_textio.all;
use std.textio.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;

entity rf_channel_bank_tb is
    generic (
        G_INPUT_FILE : string := "input.txt";
        G_OUTPUT_FILE : string := "output.txt"
    );
end entity;

architecture simulation of rf_channel_bank_tb is
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal steps : phase_steps_t := (others => (others => '0'));
    signal s_valid, s_ready : std_logic := '0';
    signal s_i, s_q : signed(15 downto 0) := (others => '0');
    signal m_valid, m_ready : std_logic := '0';
    signal m_i, m_q : iq_words_t;
    signal m_clipped : std_logic_vector(3 downto 0);
    signal m_index : unsigned(63 downto 0);
    signal done : boolean := false;
    signal accepted : natural := 0;
    signal emitted : natural := 0;
begin
    clock <= not clock after 5 ns;
    dut : entity work.rf_channel_bank
        port map (clock, reset, steps, s_valid, s_ready, s_i, s_q,
            m_valid, m_ready, m_i, m_q, m_clipped, m_index);

    producer : process
        file input_file : text open read_mode is G_INPUT_FILE;
        variable row : line;
        variable phase_word : std_logic_vector(31 downto 0);
        variable value_i, value_q : integer;
        variable count : natural := 0;
    begin
        assert not endfile(input_file) report "missing oscillator configuration" severity failure;
        readline(input_file, row);
        for channel in 0 to 3 loop
            hread(row, phase_word);
            steps(channel) <= unsigned(phase_word);
        end loop;
        for n in 0 to 4 loop wait until rising_edge(clock); end loop;
        wait until falling_edge(clock);
        reset <= '0';
        while not endfile(input_file) loop
            readline(input_file, row);
            read(row, value_i);
            read(row, value_q);
            assert value_i >= -32768 and value_i <= 32767
                and value_q >= -32768 and value_q <= 32767
                report "input component exceeds signed 16-bit range" severity failure;
            s_i <= to_signed(value_i, 16);
            s_q <= to_signed(value_q, 16);
            s_valid <= '1';
            loop
                wait until rising_edge(clock);
                exit when s_ready = '1';
            end loop;
            count := count + 1;
            accepted <= count;
            wait until falling_edge(clock);
            s_valid <= '0';
            if count mod 17 = 0 then
                wait until falling_edge(clock);
                wait until falling_edge(clock);
            end if;
        end loop;
        done <= true;
        for n in 0 to 100 loop wait until rising_edge(clock); end loop;
        assert emitted = count / 20 report "missing or extra output group" severity failure;
        report "channel-bank vector simulation passed" severity note;
        finish;
    end process;

    consumer : process(clock)
        file output_file : text open write_mode is G_OUTPUT_FILE;
        variable row : line;
        variable cycles : natural := 0;
        variable count : natural := 0;
        variable held : boolean := false;
        variable held_i, held_q : iq_words_t;
        variable held_clipped : std_logic_vector(3 downto 0);
        variable held_index : unsigned(63 downto 0);
    begin
        if rising_edge(clock) then
            cycles := cycles + 1;
            assert cycles < 20000000 report "testbench cycle budget exceeded" severity failure;
            if cycles mod 53 >= 30 and cycles mod 53 <= 48 then m_ready <= '0';
            else m_ready <= '1'; end if;
            if reset = '1' then
                assert m_valid = '0' report "reset failed to clear output valid" severity failure;
                held := false;
            else
                if held then
                    assert m_valid = '1' and m_i = held_i and m_q = held_q
                        and m_clipped = held_clipped and m_index = held_index
                        report "output changed under backpressure" severity failure;
                end if;
                held := m_valid = '1' and m_ready = '0';
                held_i := m_i; held_q := m_q;
                held_clipped := m_clipped; held_index := m_index;
                if m_valid = '1' and m_ready = '1' then
                    assert m_index = to_unsigned(count * 20 + 19, 64)
                        report "accepted-source index mismatch" severity failure;
                    write(row, to_integer(m_index(30 downto 0)));
                    for channel in 0 to 3 loop
                        write(row, string'(" ")); write(row, to_integer(m_i(channel)));
                        write(row, string'(" ")); write(row, to_integer(m_q(channel)));
                        write(row, string'(" "));
                        if m_clipped(channel) = '1' then write(row, 1);
                        else write(row, 0); end if;
                    end loop;
                    writeline(output_file, row);
                    count := count + 1;
                    emitted <= count;
                end if;
            end if;
        end if;
    end process;
end architecture;
