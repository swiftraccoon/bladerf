-- Behavioral contracts beyond numeric vectors: unstallable-ADC loss, sticky
-- configuration/source-change flags, FIFO-overflow reporting, reset while a
-- serialized record is pending, and clearing sticky state on the new epoch.
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

entity rf_channel_framer_contract_tb is end entity;
architecture simulation of rf_channel_framer_contract_tb is
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal valid : std_logic := '0';
    signal ready : std_logic := '0';
    signal source, config_error, overflow : std_logic := '0';
    signal output_valid : std_logic;
    signal output_i,output_q : signed(15 downto 0);
    signal flag_loss, flag_config, flag_clean : boolean := false;
    signal group_count : natural := 0;
begin
    clock<=not clock after 5 ns;
    dut : entity work.rf_channel_framer
        port map(clock,reset,valid,to_signed(1000,16),to_signed(-500,16),
            source,config_error,overflow,output_valid,ready,output_i,output_q);
    monitor : process(clock)
        variable slot : natural range 0 to 7 := 0;
        variable flags : std_logic_vector(15 downto 0);
    begin
        if rising_edge(clock) then
            if reset='1' then
                slot:=0; flag_loss<=false; flag_config<=false; flag_clean<=false;
                group_count<=0;
            elsif output_valid='1' and ready='1' then
                if slot=0 then
                    assert std_logic_vector(output_i)=x"5246" and std_logic_vector(output_q)=x"4f42"
                        report "lost transport record alignment" severity failure;
                elsif slot=1 then
                    flags:=std_logic_vector(output_i);
                    if flags(7)='1' then flag_loss<=true; end if;
                    if flags(6)='1' then flag_config<=true; end if;
                    if flags(7 downto 6)="00" then flag_clean<=true; end if;
                end if;
                if slot=7 then slot:=0; group_count<=group_count+1;
                else slot:=slot+1; end if;
            end if;
        end if;
    end process;
    stimulus : process
        procedure clocks(n : natural) is
        begin for k in 1 to n loop wait until falling_edge(clock); end loop; end procedure;
    begin
        clocks(3); reset<='0'; valid<='1'; source<='1';
        -- Long output backpressure overruns the unstallable ADC-facing input.
        clocks(200); ready<='1'; clocks(150);
        assert flag_loss report "ADC loss was not reported" severity failure;
        source<='0'; clocks(100);
        assert flag_config report "source change was not reported" severity failure;
        -- Reset with a record stalled partway through: old pending state clears.
        ready<='0'; clocks(13); reset<='1'; clocks(2);
        assert output_valid='0' report "reset left pending transport data" severity failure;
        source<='0'; reset<='0'; ready<='1'; clocks(100);
        assert flag_clean and not flag_loss and not flag_config
            report "reset did not establish a clean epoch" severity failure;
        overflow<='1'; clocks(1); overflow<='0'; clocks(100);
        assert flag_loss report "FIFO overflow was not reported" severity failure;
        reset<='1'; clocks(2); reset<='0'; config_error<='1'; clocks(100);
        assert flag_config report "unsupported configuration was not reported" severity failure;
        report "channel-framer reset and loss contracts passed" severity note;
        finish;
    end process;
end architecture;
