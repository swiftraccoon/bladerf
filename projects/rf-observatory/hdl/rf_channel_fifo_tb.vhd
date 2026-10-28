-- Exercise framed slots through the pinned Nuand fifo_writer's actual two-stream
-- packer, with only logical stream zero enabled. No vendor FIFO model is needed:
-- each 64-bit write is checked against two consecutive emitted 32-bit slots.
-- This verifies word order and sparse/consecutive valid handling, not FX3 USB
-- operation, physical timing closure or the complete board top level.
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;
library work;
use work.fifo_readwrite_p.all;

entity rf_channel_fifo_tb is end entity;
architecture simulation of rf_channel_fifo_tb is
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal input_valid, output_valid : std_logic := '0';
    signal output_i,output_q : signed(15 downto 0);
    signal controls : sample_controls_t(0 to 1) := (SAMPLE_CONTROL_ENABLE,SAMPLE_CONTROL_DISABLE);
    signal samples : sample_streams_t(0 to 1) := (others=>ZERO_SAMPLE);
    signal fifo_write, overflow : std_logic;
    signal fifo_data : std_logic_vector(63 downto 0);
    signal queued, checked : natural := 0;
begin
    clock<=not clock after 5 ns;
    framer : entity work.rf_channel_framer
        port map(clock,reset,input_valid,to_signed(1500,16),to_signed(-400,16),
            '1','0',overflow,output_valid,'1',output_i,output_q);
    samples(0).data_i<=output_i;
    samples(0).data_q<=output_q;
    samples(0).data_v<=output_valid;
    writer : entity work.fifo_writer
        generic map(NUM_STREAMS=>2,FIFO_USEDW_WIDTH=>12,FIFO_DATA_WIDTH=>64,
            META_FIFO_USEDW_WIDTH=>5,META_FIFO_DATA_WIDTH=>128)
        port map(clock=>clock,reset=>reset,enable=>not reset,usb_speed=>'1',
            meta_en=>'0',packet_en=>'0',eight_bit_mode_en=>'0',highly_packed_mode_en=>'0',
            timestamp=>(others=>'0'),mini_exp=>(others=>'0'),
            in_sample_controls=>controls,in_samples=>samples,
            fifo_usedw=>(others=>'0'),fifo_clear=>open,fifo_write=>fifo_write,
            fifo_full=>'0',fifo_data=>fifo_data,
            packet_control=>PACKET_CONTROL_DEFAULT,packet_ready=>open,
            meta_fifo_full=>'0',meta_fifo_usedw=>(others=>'0'),meta_fifo_data=>open,meta_fifo_write=>open,
            overflow_led=>overflow,overflow_count=>open,overflow_duration=>to_unsigned(10,16));
    checker : process(clock)
        type queue_t is array(0 to 4095) of std_logic_vector(31 downto 0);
        variable queue : queue_t;
        variable head,tail : natural := 0;
    begin
        if rising_edge(clock) then
            if reset='1' then head:=0;tail:=0;queued<=0;checked<=0;
            else
                assert overflow='0' report "unexpected packer overflow" severity failure;
                if output_valid='1' then
                    assert tail<queue'length report "test queue exhausted" severity failure;
                    queue(tail):=std_logic_vector(output_q)&std_logic_vector(output_i);
                    tail:=tail+1; queued<=tail;
                end if;
                if fifo_write='1' then
                    assert head+1<tail report "packer wrote unavailable slots" severity failure;
                    assert fifo_data=queue(head+1)&queue(head)
                        report "native FIFO packing reordered or altered framed slots" severity failure;
                    head:=head+2; checked<=head;
                end if;
            end if;
        end if;
    end process;
    stimulus : process
    begin
        for n in 0 to 3 loop wait until falling_edge(clock); end loop;
        reset<='0';input_valid<='1';
        for n in 1 to 1000 loop wait until falling_edge(clock); end loop;
        input_valid<='0';
        for n in 0 to 100 loop wait until falling_edge(clock); end loop;
        assert queued=400 and checked=400 report "native FIFO packing omitted slots" severity failure;
        report "pinned Nuand FIFO transport packing passed" severity note;
        finish;
    end process;
end architecture;
