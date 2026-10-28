-- Fixed-profile file test. Input format matches rf_channel_bank_tb; the first
-- line must contain profile-1 oscillator words. Each output line is one raw
-- transport slot, hexadecimal I Q, in transmission order. Periodic two-cycle
-- consumer stalls exercise held-slot stability without exhausting the bank.
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use ieee.std_logic_textio.all;
use std.textio.all;
use std.env.all;
library work;
use work.rf_observatory_p.all;

entity rf_channel_framer_tb is
    generic (G_INPUT_FILE : string := "input.txt"; G_OUTPUT_FILE : string := "output.txt");
end entity;

architecture simulation of rf_channel_framer_tb is
    constant STEPS : phase_steps_t := (x"b3333333",x"e6666666",x"1999999a",x"4ccccccd");
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal valid, output_valid, ready : std_logic := '0';
    signal input_i, input_q, output_i, output_q : signed(15 downto 0) := (others => '0');
    signal emitted : natural := 0;
begin
    clock <= not clock after 5 ns;
    dut : entity work.rf_channel_framer
        port map(clock,reset,valid,input_i,input_q,'0','0','0',output_valid,ready,output_i,output_q);
    producer : process
        file input_file : text open read_mode is G_INPUT_FILE;
        variable row : line;
        variable word : std_logic_vector(31 downto 0);
        variable iv,qv : integer;
        variable samples : natural := 0;
    begin
        readline(input_file,row);
        for channel in 0 to 3 loop
            hread(row,word);
            assert unsigned(word)=STEPS(channel) report "framer requires fixed profile 1" severity failure;
        end loop;
        for n in 0 to 4 loop wait until rising_edge(clock); end loop;
        wait until falling_edge(clock); reset <= '0';
        while not endfile(input_file) loop
            readline(input_file,row); read(row,iv); read(row,qv);
            assert iv>=-32768 and iv<=32767 and qv>=-32768 and qv<=32767
                report "sample outside signed16" severity failure;
            input_i<=to_signed(iv,16); input_q<=to_signed(qv,16); valid<='1';
            wait until rising_edge(clock);
            samples:=samples+1;
            wait until falling_edge(clock); valid<='0';
            if samples mod 17=0 then
                wait until falling_edge(clock); wait until falling_edge(clock);
            end if;
        end loop;
        for n in 0 to 100 loop wait until rising_edge(clock); end loop;
        assert emitted=(samples/20)*8 report "framer slot count mismatch" severity failure;
        report "channel-framer vector simulation passed" severity note;
        finish;
    end process;
    consumer : process(clock)
        file output_file : text open write_mode is G_OUTPUT_FILE;
        variable row : line;
        variable cycles,count : natural := 0;
        variable held : boolean := false;
        variable held_i,held_q : signed(15 downto 0);
    begin
        if rising_edge(clock) then
            cycles:=cycles+1;
            assert cycles<20000000 report "framer cycle budget exceeded" severity failure;
            if cycles mod 37=10 or cycles mod 37=11 then ready<='0'; else ready<='1'; end if;
            if reset='1' then held:=false;
            else
                if held then
                    assert output_valid='1' and output_i=held_i and output_q=held_q
                        report "transport slot changed under backpressure" severity failure;
                end if;
                held:=output_valid='1' and ready='0'; held_i:=output_i; held_q:=output_q;
                if output_valid='1' and ready='1' then
                    hwrite(row,std_logic_vector(output_i)); write(row,string'(" "));
                    hwrite(row,std_logic_vector(output_q)); writeline(output_file,row);
                    count:=count+1; emitted<=count;
                end if;
            end if;
        end if;
    end process;
end architecture;
